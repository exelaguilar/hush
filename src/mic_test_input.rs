//! Shared-mode WASAPI capture addressed by an exact MMDevice endpoint ID.

use anyhow::{anyhow, ensure, Context, Result};
use std::{
    ptr,
    sync::{mpsc, Arc},
    time::Duration,
};
use windows::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    Media::Audio::{
        IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    },
    System::{
        Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
            COINIT_MULTITHREADED,
        },
        Threading::WaitForMultipleObjects,
    },
};
use windows_core::{GUID, HSTRING, PCWSTR};

pub struct Capture(Arc<crate::ipc::Event>);

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct MixFormat(*mut WAVEFORMATEX);
impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

impl Capture {
    pub fn start(
        id: &str,
        samples: impl FnMut(&[f32], usize, usize) + Send + 'static,
        failure: impl FnOnce(String) + Send + 'static,
    ) -> Result<(Self, u32)> {
        let stop = Arc::new(
            crate::ipc::Event::unnamed()
                .ok_or_else(|| anyhow!("Could not create the capture stop event"))?,
        );
        let capture = Self(stop.clone());
        let id = HSTRING::from(crate::endpoints::full_device_id(id));
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("hush-mic-test".into())
            .spawn(move || {
                if let Err(error) = capture_loop(&id, &stop, &ready_tx, samples) {
                    let message = format!("{error:#}");
                    let _ = ready_tx.send(Err(message.clone()));
                    failure(message);
                }
            })
            .context("starting microphone test")?;
        let rate = ready_rx
            .recv_timeout(Duration::from_secs(3))
            .context("waiting for the microphone to start")?
            .map_err(|message| anyhow!(message))?;
        Ok((capture, rate))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.0.set();
    }
}

fn capture_loop(
    id: &HSTRING,
    stop: &crate::ipc::Event,
    ready: &mpsc::Sender<Result<u32, String>>,
    mut consume: impl FnMut(&[f32], usize, usize),
) -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let _apartment = Apartment;
    let devices: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
    let device = unsafe { devices.GetDevice(PCWSTR(id.as_ptr())) }
        .context("opening the selected microphone")?;
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }?;
    let format = MixFormat(unsafe { client.GetMixFormat() }?);
    ensure!(!format.0.is_null(), "The microphone has no shared format");
    let header = unsafe { format.0.read_unaligned() };
    let channels = usize::from(header.nChannels);
    let rate = header.nSamplesPerSec;
    let float = header.wFormatTag == 3
        || (header.wFormatTag == 0xfffe
            && header.cbSize >= 22
            && unsafe {
                ptr::addr_of!((*format.0.cast::<WAVEFORMATEXTENSIBLE>()).SubFormat).read_unaligned()
            } == GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71));
    ensure!(
        float
            && header.wBitsPerSample == 32
            && channels > 0
            && rate > 0
            && usize::from(header.nBlockAlign) == channels * 4,
        "Unsupported microphone mix format"
    );

    let audio = crate::ipc::Event::unnamed()
        .ok_or_else(|| anyhow!("Could not create the capture event"))?;
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            format.0,
            None,
        )?;
        client.SetEventHandle(audio.0)?;
    }
    let reader: IAudioCaptureClient = unsafe { client.GetService() }?;
    let max_frames = unsafe { client.GetBufferSize() }?;
    unsafe { client.Start() }.context("starting the selected microphone")?;
    if ready.send(Ok(rate)).is_err() {
        let _ = unsafe { client.Stop() };
        return Ok(());
    }
    let handles = [stop.0, audio.0];
    let result = (|| loop {
        let event = unsafe { WaitForMultipleObjects(&handles, false, 250) };
        if event == WAIT_OBJECT_0 {
            return Ok(());
        }
        ensure!(
            event == WAIT_TIMEOUT || event.0 == WAIT_OBJECT_0.0 + 1,
            "Microphone event failed"
        );
        for _ in 0..32 {
            if unsafe { reader.GetNextPacketSize() }? == 0 {
                break;
            }
            let (mut data, mut frames, mut flags) = (ptr::null_mut(), 0, 0);
            unsafe { reader.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }?;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let valid = frames <= max_frames
                && (silent || !data.is_null() && data.cast::<f32>().is_aligned());
            if valid {
                let samples = if silent {
                    &[]
                } else {
                    unsafe {
                        std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * channels)
                    }
                };
                consume(samples, channels, frames as usize);
            }
            unsafe { reader.ReleaseBuffer(frames) }?;
            ensure!(valid, "Invalid microphone buffer");
        }
    })();
    let _ = unsafe { client.Stop() };
    result
}
