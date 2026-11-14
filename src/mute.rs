//! Microphone mute: a global hotkey and the Windows capture endpoint mute.
//!
//! Muting uses the endpoint's own mute switch (the same one as Windows
//! Sound settings), so it applies in every app and never touches the audio
//! plugin or mic.conf. All COM work happens on one worker thread, which also
//! re-reads the real mute state about once per second so changes made in
//! Windows or other apps show up in Hush.

use crate::{install_state, prefs::MuteKey};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Media::Audio::{
            eCapture, eCommunications, Endpoints::IAudioEndpointVolume, IMMDevice,
            IMMDeviceEnumerator, MMDeviceEnumerator,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL,
                COINIT_MULTITHREADED,
            },
            Threading::GetCurrentThreadId,
        },
        UI::{
            Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT},
            WindowsAndMessaging::{
                GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_HOTKEY,
                WM_QUIT,
            },
        },
    },
};

enum Command {
    Toggle,
    Device(String),
    Stop,
}

#[derive(Default)]
struct Shared {
    muted: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Handle to the mute worker thread.
pub struct Mute {
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
}

impl Mute {
    /// Starts the worker. `wake` is called when the mute state changes so
    /// the UI (and tray) can update while the window is hidden.
    pub fn new(device: &str, wake: impl Fn() + Send + 'static) -> Self {
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let device = device.to_string();
        std::thread::Builder::new()
            .name("hush-mute".into())
            .spawn(move || worker(receiver, worker_shared, device, wake))
            .ok();
        Self { commands, shared }
    }

    pub fn is_muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok().and_then(|e| e.clone())
    }

    pub fn toggle(&self) {
        let _ = self.commands.send(Command::Toggle);
    }

    /// Switches the microphone the mute applies to (friendly name).
    pub fn set_device(&self, name: &str) {
        let _ = self.commands.send(Command::Device(name.to_string()));
    }

    /// A closure that toggles mute, for the hotkey thread.
    pub fn toggler(&self) -> impl Fn() + Send + 'static {
        let commands = self.commands.clone();
        move || {
            let _ = commands.send(Command::Toggle);
        }
    }
}

impl Drop for Mute {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
    }
}

fn worker(
    commands: mpsc::Receiver<Command>,
    shared: Arc<Shared>,
    mut device: String,
    wake: impl Fn() + Send + 'static,
) {
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let set_error = |text: Option<String>| {
        if let Ok(mut error) = shared.error.lock() {
            *error = text;
        }
    };
    let publish = |muted: bool| {
        if shared.muted.swap(muted, Ordering::Relaxed) != muted {
            wake();
        }
    };
    let refresh = |device: &str| match endpoint_volume(device).and_then(|v| get_mute(&v)) {
        Ok(muted) => {
            set_error(None);
            publish(muted);
        }
        Err(_) => publish(false),
    };
    refresh(&device);
    loop {
        match commands.recv_timeout(Duration::from_secs(1)) {
            Ok(Command::Toggle) => {
                let result = endpoint_volume(&device).and_then(|volume| {
                    let target = !get_mute(&volume)?;
                    unsafe { volume.SetMute(target, std::ptr::null()) }
                        .map_err(|e| e.message())?;
                    Ok(target)
                });
                match result {
                    Ok(muted) => {
                        set_error(None);
                        publish(muted);
                    }
                    Err(message) => {
                        set_error(Some(format!("Could not change mute: {message}")));
                        wake();
                    }
                }
            }
            Ok(Command::Device(name)) => {
                device = name;
                refresh(&device);
            }
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => refresh(&device),
        }
    }
    if com {
        unsafe { CoUninitialize() };
    }
}

fn get_mute(volume: &IAudioEndpointVolume) -> Result<bool, String> {
    unsafe { volume.GetMute() }
        .map(|b| b.as_bool())
        .map_err(|e| e.message())
}

/// The endpoint volume of the named microphone, or of the default
/// communications microphone when the name matches nothing.
fn endpoint_volume(name: &str) -> Result<IAudioEndpointVolume, String> {
    let failed = |e: windows::core::Error| e.message();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(failed)?;
        let device: Option<IMMDevice> = install_state::active_capture_ids(name)
            .into_iter()
            .find_map(|id| {
                // Capture endpoint ids are "{0.0.1.00000000}.{guid}".
                let full: Vec<u16> = format!("{{0.0.1.00000000}}.{id}")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                enumerator.GetDevice(PCWSTR(full.as_ptr())).ok()
            });
        let device = match device {
            Some(device) => device,
            None => enumerator
                .GetDefaultAudioEndpoint(eCapture, eCommunications)
                .map_err(failed)?,
        };
        device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .map_err(failed)
    }
}

// -- Global hotkey -------------------------------------------------------------

/// A registered global hotkey. The key is only registered while this value
/// exists; dropping it unregisters the key and stops its thread.
pub struct Hotkey {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Hotkey {
    /// Registers `key` and calls `on_press` (on the hotkey thread) for each
    /// press. Fails when another app already owns the key.
    pub fn register(key: MuteKey, on_press: impl Fn() + Send + 'static) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
        let thread = std::thread::Builder::new()
            .name("hush-hotkey".into())
            .spawn(move || {
                let id = 1;
                let registered = unsafe {
                    RegisterHotKey(
                        None,
                        id,
                        HOT_KEY_MODIFIERS(key.modifiers()) | MOD_NOREPEAT,
                        key.virtual_key(),
                    )
                };
                if let Err(error) = registered {
                    let _ = ready_tx.send(Err(error.message()));
                    return;
                }
                // Make sure the thread has a message queue before it can be
                // asked to quit.
                let mut msg = MSG::default();
                unsafe {
                    let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                }
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                loop {
                    let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                    if got.0 <= 0 || msg.message == WM_QUIT {
                        break;
                    }
                    if msg.message == WM_HOTKEY {
                        on_press();
                    }
                }
                unsafe {
                    let _ = UnregisterHotKey(None, id);
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(_)) | Err(_) => {
                let _ = thread.join();
                Err("That key is in use by another app.".into())
            }
        }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_QUIT,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
