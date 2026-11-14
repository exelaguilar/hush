//! Drives the APO through its COM interfaces in the order audiodg.exe uses:
//! DllGetClassObject -> CreateInstance -> GetRegistrationProperties ->
//! Initialize -> IsInput/OutputFormatSupported -> LockForProcess ->
//! APOProcess (many times) -> UnlockForProcess.
#![cfg(windows)]
#![allow(non_snake_case)]

use hush_apo::{DllGetClassObject, CLSID, FRIENDLY_NAME};
use std::{ffi::c_void, ptr};
use windows::Win32::{
    Media::Audio::{
        Apo::{
            APOInitBaseStruct, IAudioMediaType, IAudioMediaType_Impl, IAudioProcessingObject,
            IAudioProcessingObjectConfiguration, IAudioProcessingObjectRT, IAudioSystemEffects,
            APOERR_ALREADY_INITIALIZED, APOERR_FORMAT_NOT_SUPPORTED, APO_CONNECTION_DESCRIPTOR,
            APO_CONNECTION_PROPERTY, APO_FLAG_DEFAULT, BUFFER_SILENT, BUFFER_VALID,
            UNCOMPRESSEDAUDIOFORMAT,
        },
        WAVEFORMATEX,
    },
    System::Com::{CoTaskMemFree, IClassFactory},
};
use windows_core::{implement, IUnknown, Interface, Ref, Result, BOOL, GUID};

const FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

#[implement(IAudioMediaType)]
struct MediaType(UNCOMPRESSEDAUDIOFORMAT);

impl IAudioMediaType_Impl for MediaType_Impl {
    fn IsCompressedFormat(&self) -> Result<BOOL> {
        Ok(false.into())
    }
    fn IsEqual(&self, _other: Ref<IAudioMediaType>) -> Result<u32> {
        Ok(0)
    }
    fn GetAudioFormat(&self) -> *mut WAVEFORMATEX {
        ptr::null_mut()
    }
    fn GetUncompressedAudioFormat(&self, format: *mut UNCOMPRESSEDAUDIOFORMAT) -> Result<()> {
        unsafe { *format = self.0 };
        Ok(())
    }
}

fn media_type(subtype: GUID, bits: u32, channels: u32, rate: f32) -> IAudioMediaType {
    MediaType(UNCOMPRESSEDAUDIOFORMAT {
        guidFormatType: subtype,
        dwSamplesPerFrame: channels,
        dwBytesPerSampleContainer: bits / 8,
        dwValidBitsPerSample: bits,
        fFramesPerSecond: rate,
        dwChannelMask: 0,
    })
    .into()
}

fn factory() -> IClassFactory {
    let mut factory: *mut c_void = ptr::null_mut();
    let hr = unsafe { DllGetClassObject(&CLSID, &IClassFactory::IID, &mut factory) };
    hr.ok().expect("DllGetClassObject");
    unsafe { IClassFactory::from_raw(factory) }
}

fn create() -> IAudioProcessingObject {
    unsafe { factory().CreateInstance(None) }.expect("CreateInstance")
}

/// What audiodg.exe actually does: create the APO aggregated inside its own
/// outer object, then query the returned inner unknown for the interfaces.
#[test]
fn aggregated_like_the_audio_engine() {
    // Any COM object serves as the outer; it knows nothing about APOs.
    let outer: IUnknown = MediaType(UNCOMPRESSEDAUDIOFORMAT::default()).into();
    let factory = factory();
    unsafe {
        // COM rule: an aggregated object may only be asked for IUnknown.
        let refused: Result<IAudioProcessingObject> = factory.CreateInstance(&outer);
        assert!(refused.is_err());

        let inner: IUnknown = factory.CreateInstance(&outer).expect("aggregated create");
        let apo: IAudioProcessingObject = inner.cast().unwrap();
        let config: IAudioProcessingObjectConfiguration = inner.cast().unwrap();
        let rt: IAudioProcessingObjectRT = inner.cast().unwrap();
        let _marker: IAudioSystemEffects = inner.cast().unwrap();

        // QueryInterface on a public interface must delegate to the outer
        // object, which does not expose APO interfaces.
        assert!(apo.cast::<IAudioProcessingObjectRT>().is_err());
        assert!(
            apo.cast::<IAudioMediaType>().is_ok(),
            "outer's own interface"
        );

        exercise(&apo, &config, &rt);
        drop((apo, config, rt));
        drop(inner); // last inner reference frees the object
    }
}

#[test]
fn rejects_unknown_class_ids() {
    let mut out: *mut c_void = ptr::null_mut();
    let other = GUID::from_u128(1);
    let hr = unsafe { DllGetClassObject(&other, &IClassFactory::IID, &mut out) };
    assert!(hr.is_err());
    assert!(out.is_null());
}

#[test]
fn full_engine_lifecycle() {
    let apo = create();
    // Implements every interface the engine queries for a system effect.
    let config: IAudioProcessingObjectConfiguration = apo.cast().unwrap();
    let rt: IAudioProcessingObjectRT = apo.cast().unwrap();
    let _marker: IAudioSystemEffects = apo.cast().unwrap();
    // Not aggregated: QueryInterface reaches our own object.
    let _back: IAudioProcessingObject = rt.cast().unwrap();
    exercise(&apo, &config, &rt);
}

fn exercise(
    apo: &IAudioProcessingObject,
    config: &IAudioProcessingObjectConfiguration,
    rt: &IAudioProcessingObjectRT,
) {
    unsafe {
        let props = apo.GetRegistrationProperties().unwrap();
        assert_eq!((*props).clsid, CLSID);
        assert_eq!((*props).Flags.0, APO_FLAG_DEFAULT.0 | 1);
        assert_eq!((*props).u32NumAPOInterfaces, 1);
        assert_eq!((*props).iidAPOInterfaceList[0], IAudioProcessingObject::IID);
        let name = String::from_utf16_lossy(&(*props).szFriendlyName);
        assert!(name.starts_with(FRIENDLY_NAME));
        CoTaskMemFree(Some(props as *const c_void));

        let init = APOInitBaseStruct {
            cbSize: std::mem::size_of::<APOInitBaseStruct>() as u32,
            clsid: CLSID,
        };
        let bytes = std::slice::from_raw_parts(
            &init as *const _ as *const u8,
            std::mem::size_of::<APOInitBaseStruct>(),
        );
        apo.Initialize(bytes).unwrap();
        assert_eq!(
            apo.Initialize(bytes).unwrap_err().code(),
            APOERR_ALREADY_INITIALIZED
        );

        let float_stereo = media_type(FLOAT, 32, 2, 48_000.0);
        let pcm16 = media_type(PCM, 16, 2, 48_000.0);
        let float_44k = media_type(FLOAT, 32, 2, 44_100.0);
        assert!(apo.IsInputFormatSupported(None, &float_stereo).is_ok());
        assert!(apo
            .IsOutputFormatSupported(&float_stereo, &float_stereo)
            .is_ok());
        assert_eq!(
            apo.IsInputFormatSupported(None, &pcm16).unwrap_err().code(),
            APOERR_FORMAT_NOT_SUPPORTED
        );
        assert!(apo
            .IsOutputFormatSupported(&float_stereo, &float_44k)
            .is_err());

        const FRAMES: u32 = 480;
        let input_descriptor = APO_CONNECTION_DESCRIPTOR {
            pFormat: std::mem::ManuallyDrop::new(Some(float_stereo.clone())),
            u32MaxFrameCount: FRAMES,
            ..Default::default()
        };
        let output_descriptor = APO_CONNECTION_DESCRIPTOR {
            pFormat: std::mem::ManuallyDrop::new(Some(float_stereo.clone())),
            u32MaxFrameCount: FRAMES,
            ..Default::default()
        };
        let inputs = [&input_descriptor as *const _];
        let outputs = [&output_descriptor as *const _];
        config.LockForProcess(&inputs, &outputs).unwrap();
        assert_eq!(apo.GetInputChannelCount().unwrap(), 2);
        // 6 model frames (window, lookaheads, worker cushion) = 60 ms.
        assert_eq!(apo.GetLatency().unwrap(), 600_000);
        assert_eq!(rt.CalcOutputFrames(FRAMES), FRAMES);

        // 2 s of low-passed "room" noise, stereo, through separate buffers.
        let mut state = 0x9e37_79b9_u32;
        let mut low = 0.0_f32;
        let mut in_buf = vec![0.0_f32; (FRAMES * 2) as usize];
        let mut out_buf = vec![1.0_f32; (FRAMES * 2) as usize];
        let (mut energy_in, mut energy_out) = (0.0_f64, 0.0_f64);
        for block in 0..200 {
            for frame in in_buf.as_chunks_mut::<2>().0 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                low = 0.9 * low + (state as f32 / u32::MAX as f32 * 2.0 - 1.0);
                frame[0] = low * 0.015;
                frame[1] = low * 0.015;
            }
            let mut input = APO_CONNECTION_PROPERTY {
                pBuffer: in_buf.as_ptr() as usize,
                u32ValidFrameCount: FRAMES,
                u32BufferFlags: BUFFER_VALID,
                u32Signature: 0,
            };
            let mut output = APO_CONNECTION_PROPERTY {
                pBuffer: out_buf.as_mut_ptr() as usize,
                u32ValidFrameCount: 0,
                u32BufferFlags: BUFFER_SILENT,
                u32Signature: 0,
            };
            let in_ptrs = [&mut input as *mut _ as *const APO_CONNECTION_PROPERTY];
            let mut out_ptrs = [&mut output as *mut APO_CONNECTION_PROPERTY];
            rt.APOProcess(1, in_ptrs.as_ptr(), 1, out_ptrs.as_mut_ptr());
            // Real-time pace: the denoiser runs on a worker thread.
            std::thread::sleep(std::time::Duration::from_millis(10));
            assert_eq!(output.u32ValidFrameCount, FRAMES);
            assert_eq!(output.u32BufferFlags, BUFFER_VALID);
            assert!(out_buf.iter().all(|s| s.is_finite()));
            if block >= 100 {
                energy_in += in_buf.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
                energy_out += out_buf.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
            }
        }
        let reduction = 10.0 * (energy_out / energy_in).log10();
        assert!(reduction < -15.0, "noise only reduced by {reduction:.1} dB");

        // Silent-flagged input with garbage contents must yield finite output.
        in_buf.iter_mut().for_each(|s| *s = f32::NAN);
        let mut input = APO_CONNECTION_PROPERTY {
            pBuffer: in_buf.as_ptr() as usize,
            u32ValidFrameCount: FRAMES,
            u32BufferFlags: BUFFER_SILENT,
            u32Signature: 0,
        };
        let mut output = APO_CONNECTION_PROPERTY {
            pBuffer: out_buf.as_mut_ptr() as usize,
            ..Default::default()
        };
        let in_ptrs = [&mut input as *mut _ as *const APO_CONNECTION_PROPERTY];
        let mut out_ptrs = [&mut output as *mut APO_CONNECTION_PROPERTY];
        rt.APOProcess(1, in_ptrs.as_ptr(), 1, out_ptrs.as_mut_ptr());
        assert!(out_buf.iter().all(|s| s.is_finite()));

        // In-place processing (input and output share one buffer).
        let mut shared = vec![0.01_f32; (FRAMES * 2) as usize];
        let mut input = APO_CONNECTION_PROPERTY {
            pBuffer: shared.as_mut_ptr() as usize,
            u32ValidFrameCount: FRAMES,
            u32BufferFlags: BUFFER_VALID,
            u32Signature: 0,
        };
        let mut output = APO_CONNECTION_PROPERTY {
            pBuffer: shared.as_mut_ptr() as usize,
            ..Default::default()
        };
        let in_ptrs = [&mut input as *mut _ as *const APO_CONNECTION_PROPERTY];
        let mut out_ptrs = [&mut output as *mut APO_CONNECTION_PROPERTY];
        rt.APOProcess(1, in_ptrs.as_ptr(), 1, out_ptrs.as_mut_ptr());
        assert!(shared.iter().all(|s| s.is_finite()));

        config.UnlockForProcess().unwrap();
        assert!(config.UnlockForProcess().is_err());
        // Processing while unlocked must be a harmless no-op.
        rt.APOProcess(1, in_ptrs.as_ptr(), 1, out_ptrs.as_mut_ptr());

        std::mem::ManuallyDrop::into_inner(input_descriptor.pFormat);
        std::mem::ManuallyDrop::into_inner(output_descriptor.pFormat);
    }
}
