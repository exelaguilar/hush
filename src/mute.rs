//! Microphone mute: a global hotkey and the Windows capture endpoint mute.
//!
//! Muting uses the endpoint's own mute switch (the same one as Windows
//! Sound settings), so it applies in every app and never touches the audio
//! plugin or mic.conf. All COM work happens on one worker thread. It keeps the
//! endpoint volume interface of the chosen microphone and listens for mute
//! changes made in Windows or other apps; a slow refresh is only a safety net.

use crate::{endpoints, prefs::MuteKey};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
use windows::{
    core::{implement, PCWSTR},
    Win32::{
        Media::Audio::{
            Endpoints::{
                IAudioEndpointVolume, IAudioEndpointVolumeCallback,
                IAudioEndpointVolumeCallback_Impl,
            },
            IMMDeviceEnumerator, MMDeviceEnumerator, AUDIO_VOLUME_NOTIFICATION_DATA,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
            },
            Threading::GetCurrentThreadId,
        },
        UI::{
            Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
            },
            WindowsAndMessaging::{
                GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_HOTKEY, WM_QUIT,
            },
        },
    },
};

/// Safety net in case a change notification is ever missed.
const FALLBACK_REFRESH: Duration = Duration::from_secs(10);
const NOT_FOUND: &str = "Microphone not found.";

enum Command {
    Toggle,
    Device(String),
    /// Devices were added or removed: look the microphone up again.
    Rescan,
    Stop,
}

type Wake = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    muted: AtomicBool,
    error: Mutex<Option<String>>,
    wake: Wake,
}

impl Shared {
    fn publish(&self, muted: bool) {
        if self.muted.swap(muted, Ordering::Relaxed) != muted {
            (self.wake)();
        }
    }

    fn set_error(&self, text: Option<String>) {
        if let Ok(mut error) = self.error.lock() {
            *error = text;
        }
    }
}

/// Handle to the mute worker thread.
pub struct Mute {
    commands: mpsc::Sender<Command>,
    shared: Arc<Shared>,
}

impl Mute {
    /// Starts the worker. `wake` is called when the mute state changes so
    /// the UI (and tray) can update while the window is hidden.
    pub fn new(device: &str, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            muted: AtomicBool::new(false),
            error: Mutex::new(None),
            wake: Arc::new(wake),
        });
        let worker_shared = shared.clone();
        let device = device.to_string();
        std::thread::Builder::new()
            .name("hush-mute".into())
            .spawn(move || worker(receiver, worker_shared, device))
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

    /// Looks the microphone up again (after devices were added or removed).
    pub fn rescan(&self) {
        let _ = self.commands.send(Command::Rescan);
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

/// Receives mute changes from Windows (on a COM thread of its own).
#[implement(IAudioEndpointVolumeCallback)]
struct VolumeEvents {
    shared: Arc<Shared>,
}

#[allow(non_snake_case)]
impl IAudioEndpointVolumeCallback_Impl for VolumeEvents_Impl {
    fn OnNotify(&self, pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if let Some(data) = unsafe { pnotify.as_ref() } {
            self.shared.publish(data.bMuted.as_bool());
        }
        Ok(())
    }
}

/// The endpoint volume of the selected microphone with our change callback.
struct Endpoint {
    volume: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
}

impl Endpoint {
    /// Opens the selected endpoint by ID. There is no
    /// fallback to another device: muting the wrong microphone is worse than
    /// reporting that the right one is missing.
    fn open(
        enumerator: &IMMDeviceEnumerator,
        id: &str,
        shared: &Arc<Shared>,
    ) -> Result<Self, String> {
        let failed = |e: windows::core::Error| e.message();
        let full = windows_core::HSTRING::from(endpoints::full_device_id(id));
        let device = unsafe { enumerator.GetDevice(PCWSTR(full.as_ptr())) }
            .map_err(|_| NOT_FOUND.to_string())?;
        if !unsafe { device.GetState() }.is_ok_and(|state| state.0 == 1) {
            return Err(NOT_FOUND.to_string());
        }
        let volume =
            unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }.map_err(failed)?;
        let callback: IAudioEndpointVolumeCallback = VolumeEvents {
            shared: shared.clone(),
        }
        .into();
        // Without the callback the slow refresh still works.
        let _ = unsafe { volume.RegisterControlChangeNotify(&callback) };
        Ok(Self { volume, callback })
    }

    fn muted(&self) -> Result<bool, String> {
        unsafe { self.volume.GetMute() }
            .map(|b| b.as_bool())
            .map_err(|e| e.message())
    }

    fn set_muted(&self, muted: bool) -> Result<(), String> {
        unsafe { self.volume.SetMute(muted, std::ptr::null()) }.map_err(|e| e.message())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = unsafe { self.volume.UnregisterControlChangeNotify(&self.callback) };
    }
}

struct Worker {
    enumerator: Option<IMMDeviceEnumerator>,
    shared: Arc<Shared>,
    device: String,
    current: Option<Endpoint>,
}

impl Worker {
    /// Resolves the microphone again, replacing the cached endpoint.
    fn resolve(&mut self) -> Result<(), String> {
        self.current = None;
        if self.enumerator.is_none() {
            self.enumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok();
        }
        let enumerator = self
            .enumerator
            .as_ref()
            .ok_or_else(|| "Windows audio is not available.".to_string())?;
        self.current = match Endpoint::open(enumerator, &self.device, &self.shared) {
            Ok(endpoint) => Some(endpoint),
            Err(error) => {
                self.enumerator = None;
                return Err(error);
            }
        };
        Ok(())
    }

    /// Reads the real mute state and publishes it. Not muted when the
    /// microphone cannot be found.
    fn refresh(&mut self) {
        let read = |worker: &Worker| worker.current.as_ref().map(Endpoint::muted);
        let mut result = read(self);
        // A failed call usually means the device was replugged: look again.
        if !matches!(result, Some(Ok(_))) && self.resolve().is_ok() {
            result = read(self);
        }
        match result {
            Some(Ok(muted)) => {
                self.shared.set_error(None);
                self.shared.publish(muted);
            }
            _ => self.shared.publish(false),
        }
    }

    fn toggle(&mut self) {
        let mut result = self.try_toggle();
        if result.is_err() && self.current.is_some() {
            // The cached endpoint may be stale: look again and retry once.
            result = match self.resolve() {
                Ok(()) => self.try_toggle(),
                Err(message) => Err(message),
            };
        }
        match result {
            Ok(muted) => {
                self.shared.set_error(None);
                self.shared.publish(muted);
            }
            Err(message) => {
                let text = if message == NOT_FOUND {
                    message
                } else {
                    format!("Could not change mute: {message}")
                };
                self.shared.set_error(Some(text));
                (self.shared.wake)();
            }
        }
    }

    fn try_toggle(&mut self) -> Result<bool, String> {
        if self.current.is_none() {
            self.resolve()?;
        }
        let endpoint = self.current.as_ref().ok_or_else(|| NOT_FOUND.to_string())?;
        let target = !endpoint.muted()?;
        endpoint.set_muted(target)?;
        Ok(target)
    }
}

fn worker(commands: mpsc::Receiver<Command>, shared: Arc<Shared>, device: String) {
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let mut worker = Worker {
        enumerator: None,
        shared,
        device,
        current: None,
    };
    worker.refresh();
    loop {
        match commands.recv_timeout(FALLBACK_REFRESH) {
            Ok(Command::Toggle) => worker.toggle(),
            Ok(Command::Device(name)) => {
                worker.device = name;
                worker.shared.set_error(None);
                // Dropping the cached endpoint makes refresh look it up.
                worker.current = None;
                worker.refresh();
            }
            Ok(Command::Rescan) => {
                worker.current = None;
                worker.refresh();
            }
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => worker.refresh(),
        }
    }
    // COM objects must be released before COM is shut down.
    worker.current = None;
    worker.enumerator = None;
    if com {
        unsafe { CoUninitialize() };
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
        let (go_tx, go_rx) = mpsc::channel();
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
                if ready_tx.send(Ok(unsafe { GetCurrentThreadId() })).is_err()
                    || go_rx.recv().is_err()
                {
                    unsafe {
                        let _ = UnregisterHotKey(None, id);
                    }
                    return;
                }
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
            Ok(Ok(thread_id)) => {
                go_tx
                    .send(())
                    .map_err(|_| "Hotkey worker stopped.".to_string())?;
                Ok(Self {
                    thread_id,
                    thread: Some(thread),
                })
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err("Hotkey registration did not finish.".into()),
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
