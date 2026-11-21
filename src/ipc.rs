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

use crate::health::{ActionKind, Health};
use std::path::PathBuf;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
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

/// An auto-reset event owned by this process.
pub struct Event(pub HANDLE);

// The handle is only waited on and set, which is thread-safe.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub fn create(name: &str) -> Option<Event> {
        let name = HSTRING::from(name);
        unsafe { CreateEventW(None, false, false, PCWSTR(name.as_ptr())) }
            .ok()
            .map(Event)
    }

    /// An unnamed event, for waking a wait loop from other threads.
    pub fn unnamed() -> Option<Event> {
        unsafe { CreateEventW(None, false, false, None) }.ok().map(Event)
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
                unsafe {
                    windows::Win32::System::Threading::WaitForSingleObject(
                        event.0,
                        windows::Win32::System::Threading::INFINITE,
                    )
                };
                on_set();
            }
        })
        .ok();
}

/// Asks the agent to run a repair (the user clicked Fix now). False if the
/// agent is not running.
pub fn request_repair(kind: ActionKind) -> bool {
    // The repair may fall back to an administrator prompt from the agent.
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
            windows::Win32::UI::WindowsAndMessaging::ASFW_ANY,
        );
    }
    signal(match kind {
        ActionKind::Repair => EV_REPAIR,
        ActionKind::Restart => EV_RESTART,
    })
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

// -- Agent state ----------------------------------------------------------------------

/// What the agent knows that the window shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentState {
    pub health: Health,
    /// The microphone whose health is watched.
    pub target: String,
    /// A repair is running.
    pub repairing: bool,
    /// Message of the last failed repair.
    pub repair_note: Option<String>,
    /// Why the mute hotkey could not be registered.
    pub hotkey_error: Option<String>,
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
            "health = {health}\nwrong_app = {}\nwrong_other = {}\ntarget = {}\nrepairing = {}\nrepair_note = {}\nhotkey_error = {}\napps = {}\n",
            one_line(app),
            one_line(other),
            one_line(&self.target),
            if self.repairing { "1" } else { "0" },
            one_line(self.repair_note.as_deref().unwrap_or("")),
            one_line(self.hotkey_error.as_deref().unwrap_or("")),
            self.apps.iter().map(|a| one_line(a).replace('|', " ")).collect::<Vec<_>>().join("|"),
        )
    }

    pub fn parse(text: &str) -> AgentState {
        let mut state = AgentState::default();
        let (mut health, mut app, mut other) = (String::new(), String::new(), String::new());
        let some = |value: &str| Some(value.to_string()).filter(|v| !v.is_empty());
        for line in text.lines() {
            let Some((key, value)) = line.split_once(" = ").or_else(|| {
                line.strip_suffix(" =").map(|key| (key, ""))
            }) else {
                continue;
            };
            match key {
                "health" => health = value.to_string(),
                "wrong_app" => app = value.to_string(),
                "wrong_other" => other = value.to_string(),
                "target" => state.target = value.to_string(),
                "repairing" => state.repairing = value == "1",
                "repair_note" => state.repair_note = some(value),
                "hotkey_error" => state.hotkey_error = some(value),
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
        let temp = path.with_extension("state.tmp");
        std::fs::write(&temp, self.to_text())?;
        std::fs::rename(&temp, &path)
    }

    pub fn read() -> Option<AgentState> {
        std::fs::read_to_string(state_path())
            .ok()
            .map(|text| AgentState::parse(&text))
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
                repairing: true,
                repair_note: Some("The repair did not\nfinish in time.".into()),
                hotkey_error: Some("Ctrl+Alt+M is in use".into()),
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
    fn garbage_is_healthy_and_empty() {
        assert_eq!(AgentState::parse("nonsense\nhealth = ???\n"), AgentState::default());
    }
}
