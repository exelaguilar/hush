//! How the two Hush processes talk.
//!
//! * The **tray agent** (`hush.exe`) runs all the time: tray icon, health
//!   watch and repairs, mute hotkey, notifications. It never loads graphics.
//! * The **window** (`hush.exe --window`) runs only while it is open.
//!
//! They already share `mic.conf` (settings) and `app.conf` (preferences)
//! and watch each other's writes. On top of that the agent publishes its
//! state to `agent.state`, and both sides signal each other with named
//! events in the user's session.

use crate::health::Health;
use std::path::PathBuf;
use windows::Win32::{
    Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE},
    Security::SECURITY_ATTRIBUTES,
    System::Threading::{CreateEventW, OpenEventW, SetEvent, EVENT_MODIFY_STATE},
};
use windows_core::{HSTRING, PCWSTR};

/// Command-line flag that starts the window process.
pub const WINDOW_FLAG: &str = "--window";
/// `--page <id>` opens the window on a page.
pub const PAGE_FLAG: &str = "--page";

/// Named events. Agent: creates the first five and waits on them. Window:
/// creates the last two.
pub const EV_SHOW: &str = r"Local\Hush.Show";
pub const EV_REPAIR: &str = r"Local\Hush.Repair";
pub const EV_RESTART: &str = r"Local\Hush.Restart";
pub const EV_QUIT: &str = r"Local\Hush.Quit";
pub const EV_PREFS: &str = r"Local\Hush.Prefs";
pub const EV_WINDOW_SHOW: &str = r"Local\Hush.WindowShow";
pub const EV_WINDOW_CLOSE: &str = r"Local\Hush.WindowClose";

struct EventSecurity(*mut std::ffi::c_void);

impl EventSecurity {
    fn new() -> Option<Self> {
        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text: *const u16,
                revision: u32,
                descriptor: *mut *mut std::ffi::c_void,
                size: *mut u32,
            ) -> i32;
        }
        // Owner Rights applies to the creating user's owner SID; Local limits
        // the name to the interactive session.
        let sddl = windows_core::w!("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
        let mut descriptor = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(Self(descriptor))
    }
}

impl Drop for EventSecurity {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(Some(
                    windows::Win32::Foundation::HLOCAL(self.0),
                ));
            }
        }
    }
}

/// An auto-reset event owned by this process.
pub struct Event(pub HANDLE);

// The handle is only waited on and set, which is thread-safe.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    /// Creates a fresh named event. An existing one is not adopted (it may
    /// carry someone else's security), but a process that is merely
    /// signaling or probing a previous instance's event holds it for
    /// microseconds, so creation is retried for a moment before giving up.
    pub fn create(name: &str) -> Option<Event> {
        Self::create_within(name, std::time::Duration::from_secs(2))
    }

    fn create_within(name: &str, wait: std::time::Duration) -> Option<Event> {
        let deadline = std::time::Instant::now() + wait;
        let name = HSTRING::from(name);
        let security = EventSecurity::new()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: false.into(),
        };
        loop {
            let handle =
                unsafe { CreateEventW(Some(&attributes), false, false, PCWSTR(name.as_ptr())) }
                    .ok()?;
            if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                return Some(Event(handle));
            }
            unsafe {
                let _ = CloseHandle(handle);
            }
            if std::time::Instant::now() >= deadline {
                hush_apo::settings::trace(&format!("ipc: {name} is held by another process"));
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// An unnamed event, for waking a wait loop from other threads.
    pub fn unnamed() -> Option<Event> {
        unsafe { CreateEventW(None, false, false, None) }
            .ok()
            .map(Event)
    }

    pub fn set(&self) {
        unsafe {
            let _ = SetEvent(self.0);
        }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Sets another process's event. False if nobody created it, which means
/// that process is not running.
pub fn signal(name: &str) -> bool {
    let name = HSTRING::from(name);
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) {
            Ok(event) => {
                let ok = SetEvent(event).is_ok();
                let _ = CloseHandle(event);
                ok
            }
            Err(_) => false,
        }
    }
}

/// Whether the tray agent is running.
pub fn agent_running() -> bool {
    exists(EV_SHOW)
}

/// Whether the window process is running.
pub fn window_running() -> bool {
    exists(EV_WINDOW_SHOW)
}

fn exists(name: &str) -> bool {
    let name = HSTRING::from(name);
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) {
            Ok(event) => {
                let _ = CloseHandle(event);
                true
            }
            Err(_) => false,
        }
    }
}

/// Creates the named event and runs `on_set` on a thread of its own each
/// time another process sets it.
pub fn on_event(name: &str, on_set: impl Fn() + Send + 'static) {
    let Some(event) = Event::create(name) else {
        return;
    };
    std::thread::Builder::new()
        .name("hush-event".into())
        .spawn(move || {
            // Capture the whole Event (Send), not its raw handle field.
            let event = event;
            loop {
                let result = unsafe {
                    windows::Win32::System::Threading::WaitForSingleObject(
                        event.0,
                        windows::Win32::System::Threading::INFINITE,
                    )
                };
                if result != windows::Win32::Foundation::WAIT_OBJECT_0 {
                    return;
                }
                on_set();
            }
        })
        .ok();
}

fn page_request_path() -> PathBuf {
    crate::prefs::dir().join("open-page")
}

/// Leaves a page for the running window to switch to on its next show.
pub fn request_page(page: &str) {
    let path = page_request_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, page);
}

/// Takes the page left by `request_page`, if any.
pub fn take_page_request() -> Option<String> {
    let path = page_request_path();
    let page = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(page.trim().to_string()).filter(|p| !p.is_empty())
}

fn process_running(pid: u32) -> bool {
    use windows::Win32::{
        Foundation::WAIT_TIMEOUT,
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    let Ok(process) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return false;
    };
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    unsafe {
        let _ = CloseHandle(process);
    }
    running
}

// -- Agent state ----------------------------------------------------------------------

/// What the agent knows that the window shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentState {
    pub health: Health,
    /// The microphone whose health is watched.
    pub target: String,
    pub target_id: String,
    pub process_id: u32,
    /// A repair is running.
    pub repairing: bool,
    /// Message of the last failed repair.
    pub repair_note: Option<String>,
    /// Why the mute hotkey could not be registered.
    pub hotkey_error: Option<String>,
    /// Why the Clean Mic hotkey could not be registered.
    pub toggle_hotkey_error: Option<String>,
    /// Apps capturing from the watched microphone right now.
    pub apps: Vec<String>,
}

pub fn state_path() -> PathBuf {
    crate::prefs::dir().join("agent.state")
}

fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

impl AgentState {
    pub fn to_text(&self) -> String {
        let (health, app, other) = match &self.health {
            Health::WrongMic { app, other } => ("wrong_mic", app.as_str(), other.as_str()),
            health => (health.id(), "", ""),
        };
        format!(
            "health = {health}\nwrong_app = {}\nwrong_other = {}\ntarget = {}\ntarget_id = {}\nprocess_id = {}\nrepairing = {}\nrepair_note = {}\nhotkey_error = {}\ntoggle_hotkey_error = {}\napps = {}\n",
            one_line(app),
            one_line(other),
            one_line(&self.target),
            one_line(&self.target_id),
            self.process_id,
            if self.repairing { "1" } else { "0" },
            one_line(self.repair_note.as_deref().unwrap_or("")),
            one_line(self.hotkey_error.as_deref().unwrap_or("")),
            one_line(self.toggle_hotkey_error.as_deref().unwrap_or("")),
            self.apps.iter().map(|a| one_line(a).replace('|', " ")).collect::<Vec<_>>().join("|"),
        )
    }

    pub fn parse(text: &str) -> AgentState {
        let mut state = AgentState::default();
        let (mut health, mut app, mut other) = (String::new(), String::new(), String::new());
        let some = |value: &str| Some(value.to_string()).filter(|v| !v.is_empty());
        for line in text.lines() {
            let Some((key, value)) = line
                .split_once(" = ")
                .or_else(|| line.strip_suffix(" =").map(|key| (key, "")))
            else {
                continue;
            };
            match key {
                "health" => health = value.to_string(),
                "wrong_app" => app = value.to_string(),
                "wrong_other" => other = value.to_string(),
                "target" => state.target = value.to_string(),
                "target_id" => state.target_id = value.to_string(),
                "process_id" => state.process_id = value.parse().unwrap_or(0),
                "repairing" => state.repairing = value == "1",
                "repair_note" => state.repair_note = some(value),
                "hotkey_error" => state.hotkey_error = some(value),
                "toggle_hotkey_error" => state.toggle_hotkey_error = some(value),
                "apps" => {
                    state.apps = value
                        .split('|')
                        .filter(|a| !a.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                _ => {}
            }
        }
        state.health = if health == "wrong_mic" {
            Health::WrongMic { app, other }
        } else {
            Health::from_id(&health).unwrap_or(Health::Healthy)
        };
        state
    }

    /// Written to a temporary file and renamed, so a reader never sees half.
    pub fn write(&self) -> std::io::Result<()> {
        let path = state_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        hush_apo::settings::write_atomic(&path, &self.to_text())
    }

    pub fn read() -> Option<AgentState> {
        hush_apo::settings::read_config(&state_path())
            .ok()
            .map(|text| AgentState::parse(&text))
            .filter(|state| state.process_id != 0 && process_running(state.process_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_round_trips() {
        let states = [
            AgentState::default(),
            AgentState {
                health: Health::NotProcessing,
                target: "Microphone (fifine SC3)".into(),
                target_id: "{test}".into(),
                process_id: 42,
                repairing: true,
                repair_note: Some("The repair did not\nfinish in time.".into()),
                hotkey_error: Some("Ctrl+Alt+M is in use".into()),
                toggle_hotkey_error: Some("Pick a different key".into()),
                apps: vec!["Discord".into(), "OBS".into()],
            },
            AgentState {
                health: Health::WrongMic {
                    app: "Discord".into(),
                    other: "Headset (USB)".into(),
                },
                ..AgentState::default()
            },
        ];
        for state in states {
            let mut expected = state.clone();
            expected.repair_note = expected.repair_note.map(|n| one_line(&n));
            assert_eq!(AgentState::parse(&state.to_text()), expected);
        }
    }

    #[test]
    fn every_health_id_parses_back() {
        for health in [
            Health::Healthy,
            Health::Disconnected,
            Health::Missing,
            Health::NotProcessing,
            Health::Bypassed,
            Health::Failing,
            Health::EnhancementsOff,
        ] {
            assert_eq!(Health::from_id(health.id()), Some(health.clone()));
        }
    }

    #[test]
    fn named_event_is_created_once_a_stale_handle_closes() {
        let name = format!(r"Local\Hush.Test.{}", std::process::id());
        let first = Event::create_within(&name, std::time::Duration::ZERO).unwrap();
        // Still open elsewhere: never adopted.
        assert!(Event::create_within(&name, std::time::Duration::from_millis(50)).is_none());
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(first);
        });
        assert!(Event::create_within(&name, std::time::Duration::from_secs(5)).is_some());
        release.join().unwrap();
    }

    #[test]
    fn garbage_is_healthy_and_empty() {
        assert_eq!(
            AgentState::parse("nonsense\nhealth = ???\n"),
            AgentState::default()
        );
    }
}
