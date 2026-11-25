//! The tray agent: the part of Hush that runs all the time.
//!
//! It owns the tray icon, the microphone health watch and its repairs, the
//! global mute hotkey and notifications. It never creates a window or loads
//! graphics, so it stays small; `hush.exe --window` is started on demand
//! and exits when closed (see `ipc`).
//!
//! The main thread waits on named events (from the window and later
//! launches), an unnamed wake event (from worker threads) and its message
//! queue (the tray icon), and runs `tick` at least once a second.

use crate::{
    endpoints, health, install_state, ipc, mute, notify, prefs,
    tray::{Tray, TrayAction},
};
use hush_apo::settings;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant, SystemTime},
};
use windows::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0},
    UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW,
        TranslateMessage, ASFW_ANY, MSG, PM_REMOVE, QS_ALLINPUT, WM_QUIT,
    },
};

/// How long a freshly started window process counts as running before its
/// "show" event exists (so a double click does not start two).
const SPAWN_GRACE: Duration = Duration::from_secs(5);

struct Health {
    watch: health::Watcher,
    monitor: health::Monitor,
    current: health::Health,
    probe: Option<health::Probe>,
    target: String,
    repair_job: Option<(health::ActionKind, mpsc::Receiver<health::Outcome>)>,
    repair_note: Option<String>,
}

struct Agent {
    enabled: bool,
    settings_mtime: Option<SystemTime>,
    prefs: prefs::Prefs,
    prefs_mtime: Option<SystemTime>,
    state: install_state::InstallState,
    state_checked: Instant,
    tray: Option<Tray>,
    mute: mute::Mute,
    mute_mic: String,
    hotkey: Option<mute::Hotkey>,
    hotkey_error: Option<String>,
    /// Optional hotkey that turns Clean Mic on and off.
    toggle_hotkey: Option<mute::Hotkey>,
    toggle_error: Option<String>,
    /// Set on the hotkey thread; the main loop does the toggle.
    toggle_pressed: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
    live: settings::LiveReader,
    health: Health,
    published: Option<ipc::AgentState>,
    window: Option<(std::process::Child, Instant)>,
    quitting: bool,
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The microphone whose health is watched: the first one Clean Mic is
/// installed on.
fn health_target(state: &install_state::InstallState) -> String {
    state.attached.first().cloned().unwrap_or_default()
}

/// The microphone the mute hotkey acts on: the one Clean Mic is installed
/// on, else the first plugged-in microphone.
fn mute_target(state: &install_state::InstallState) -> String {
    state.attached.first().cloned().unwrap_or_else(|| {
        endpoints::capture_endpoints()
            .into_iter()
            .find(|e| e.active)
            .map(|e| e.name)
            .unwrap_or_default()
    })
}

impl Agent {
    fn new(wake: impl Fn() + Send + Sync + Clone + 'static) -> Self {
        let settings = settings::load();
        let state = install_state::read();
        let target = health_target(&state);
        let mute_mic = mute_target(&state);
        let mute = mute::Mute::new(&mute_mic, wake.clone());
        let watch = health::Watcher::new(&target, wake.clone());
        // Toast identity (name, icon, click handler) for notifications.
        std::thread::spawn(notify::register);
        let wake_for_toggle = wake.clone();
        let tray = Tray::new(settings.enabled, false, wake);
        let mut agent = Self {
            enabled: settings.enabled,
            settings_mtime: mtime(&settings::settings_path()),
            prefs: prefs::load(),
            prefs_mtime: mtime(&prefs::path()),
            state,
            state_checked: Instant::now(),
            tray,
            mute,
            mute_mic,
            hotkey: None,
            hotkey_error: None,
            toggle_hotkey: None,
            toggle_error: None,
            toggle_pressed: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(wake_for_toggle),
            live: settings::LiveReader::new(),
            health: Health {
                watch,
                monitor: health::Monitor::default(),
                current: health::Health::Healthy,
                probe: None,
                target,
                repair_job: None,
                repair_note: None,
            },
            published: None,
            window: None,
            quitting: false,
        };
        agent.apply_hotkey();
        agent
    }

    /// (Re)registers the global hotkeys to match the preferences.
    fn apply_hotkey(&mut self) {
        // Unregister first so a changed key never leaves the old one active.
        self.hotkey = None;
        self.hotkey_error = None;
        self.toggle_hotkey = None;
        self.toggle_error = None;
        if self.prefs.mute_hotkey {
            match mute::Hotkey::register(self.prefs.mute_key, self.mute.toggler()) {
                Ok(hotkey) => self.hotkey = Some(hotkey),
                Err(error) => self.hotkey_error = Some(error),
            }
        }
        if self.prefs.toggle_hotkey {
            if self.prefs.mute_hotkey && self.prefs.toggle_key == self.prefs.mute_key {
                self.toggle_error = Some("The mute hotkey already uses that key.".into());
                return;
            }
            let pressed = self.toggle_pressed.clone();
            let wake = self.wake.clone();
            let on_press = move || {
                pressed.store(true, Ordering::Release);
                wake();
            };
            match mute::Hotkey::register(self.prefs.toggle_key, on_press) {
                Ok(hotkey) => self.toggle_hotkey = Some(hotkey),
                Err(error) => self.toggle_error = Some(error),
            }
        }
    }

    fn reload_prefs(&mut self) {
        self.prefs_mtime = mtime(&prefs::path());
        let loaded = prefs::load();
        let hotkey_changed = loaded.mute_hotkey != self.prefs.mute_hotkey
            || loaded.mute_key != self.prefs.mute_key
            || loaded.toggle_hotkey != self.prefs.toggle_hotkey
            || loaded.toggle_key != self.prefs.toggle_key;
        self.prefs = loaded;
        if hotkey_changed {
            self.apply_hotkey();
        }
    }

    fn refresh_install_state(&mut self) {
        self.state = install_state::read();
        self.state_checked = Instant::now();
        let mic = mute_target(&self.state);
        if mic != self.mute_mic {
            self.mute.set_device(&mic);
            self.mute_mic = mic;
        }
    }

    /// Shows the window, starting the window process if needed.
    fn open_window(&mut self, page: Option<&str>) {
        let starting = self.window.as_mut().is_some_and(|(child, at)| {
            at.elapsed() < SPAWN_GRACE && matches!(child.try_wait(), Ok(None))
        });
        if ipc::window_running() || starting {
            // This process just got the click; let the window take focus.
            unsafe {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
            }
            if let Some(page) = page {
                ipc::request_page(page);
            }
            ipc::signal(ipc::EV_WINDOW_SHOW);
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        unsafe {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
        }
        let mut command = std::process::Command::new(exe);
        command.arg(ipc::WINDOW_FLAG);
        if let Some(page) = page {
            command.args([ipc::PAGE_FLAG, page]);
        }
        match command.spawn() {
            Ok(child) => self.window = Some((child, Instant::now())),
            Err(error) => settings::trace(&format!("agent: could not open the window: {error}")),
        }
    }

    fn toggle_clean_mic(&mut self) {
        let mut current = settings::load();
        current.enabled = !current.enabled;
        if settings::save(&current).is_ok() {
            self.enabled = current.enabled;
            self.settings_mtime = mtime(&settings::settings_path());
        }
    }

    fn start_repair(&mut self, kind: health::ActionKind, allow_uac: bool, manual: bool) {
        if self.health.repair_job.is_some() {
            return;
        }
        if manual {
            self.health.monitor.manual_started();
        }
        self.health.repair_note = None;
        self.health.repair_job = Some((kind, health::run_repair(kind, allow_uac)));
    }

    fn poll_health(&mut self, live_age_ms: Option<u64>) {
        let now = Instant::now();
        let target = health_target(&self.state);
        if target != self.health.target {
            self.health.watch.set_target(&target);
            self.health.target = target;
        }
        if let Some(update) = self.health.watch.poll() {
            self.health.probe = Some(update.probe);
            if update.devices_changed {
                self.refresh_install_state();
                self.mute.rescan();
            }
        }
        let finished = match &self.health.repair_job {
            Some((kind, job)) => match job.try_recv() {
                Ok(outcome) => Some((*kind, Some(outcome))),
                Err(mpsc::TryRecvError::Disconnected) => Some((*kind, None)),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        if let Some((kind, outcome)) = finished {
            self.health.repair_job = None;
            let success = outcome.as_ref().is_some_and(|o| o.success);
            self.health.monitor.action_finished(kind, success, now);
            if !success {
                self.health.repair_note = Some(
                    outcome
                        .map(|o| o.message)
                        .unwrap_or_else(|| "The repair did not finish.".into()),
                );
            }
            self.refresh_install_state();
        }
        let Some(probe) = self.health.probe.clone() else {
            return;
        };
        let snapshot = health::Snapshot {
            mic: self.health.target.clone(),
            installed: !self.state.attached.is_empty(),
            // mic.conf as written, so "Hold to hear original" counts as off.
            clean_mic_on: self.enabled,
            probe,
            live_age_ms,
        };
        let config = health::Config {
            auto_repair: self.prefs.auto_repair,
            hold_pressed: false,
        };
        let step = self.health.monitor.step(&snapshot, config, now);
        self.health.current = step.health;
        if let Some(kind) = step.action {
            // Automatic repairs never pop UAC; only "Fix now" may.
            self.start_repair(kind, false, false);
        }
        if let (Some(notice), true) = (step.notice, self.prefs.notifications) {
            let (title, text) = notice.text();
            notify::show(&title, &text);
        }
    }

    fn status(&self) -> &'static str {
        if self.state.attached.is_empty() {
            "Not installed"
        } else if !self.enabled {
            "Off"
        } else if self.state.running {
            "Active"
        } else {
            "Ready (mic idle)"
        }
    }

    fn sync_tray(&self) {
        let Some(tray) = &self.tray else {
            return;
        };
        let muted = self.mute.is_muted();
        let attention = self.health.current.needs_attention();
        tray.sync(self.enabled, muted, attention);
        if attention && !muted {
            tray.set_tooltip(&format!("Hush \u{2013} {}", self.health.current.short()));
        } else if muted {
            tray.set_tooltip("Hush \u{2013} microphone muted");
        } else {
            tray.set_tooltip(&format!("Hush \u{2013} Clean Mic: {}", self.status()));
        }
    }

    fn publish(&mut self) {
        let state = ipc::AgentState {
            health: self.health.current.clone(),
            target: self.health.target.clone(),
            repairing: self.health.repair_job.is_some(),
            repair_note: self.health.repair_note.clone(),
            hotkey_error: self.hotkey_error.clone(),
            toggle_hotkey_error: self.toggle_error.clone(),
            apps: self
                .health
                .probe
                .as_ref()
                .map(|p| p.apps.clone())
                .unwrap_or_default(),
        };
        if self.published.as_ref() != Some(&state) {
            if let Err(error) = state.write() {
                settings::trace(&format!("agent: could not write its state: {error}"));
            }
            self.published = Some(state);
        }
    }

    fn handle_tray(&mut self) {
        let actions: Vec<TrayAction> = match &self.tray {
            Some(tray) => std::iter::from_fn(|| tray.poll()).collect(),
            None => Vec::new(),
        };
        for action in actions {
            match action {
                TrayAction::Open => self.open_window(None),
                TrayAction::OpenMicTest => self.open_window(Some("test")),
                TrayAction::ToggleCleanMic => self.toggle_clean_mic(),
                TrayAction::ToggleMute => self.mute.toggle(),
                TrayAction::Quit => self.quitting = true,
            }
        }
    }

    fn tick(&mut self) {
        self.handle_tray();
        if self.toggle_pressed.swap(false, Ordering::AcqRel) {
            self.toggle_clean_mic();
        }
        let settings_mtime = mtime(&settings::settings_path());
        if settings_mtime != self.settings_mtime {
            self.settings_mtime = settings_mtime;
            self.enabled = settings::load().enabled;
        }
        if mtime(&prefs::path()) != self.prefs_mtime {
            self.reload_prefs();
        }
        // Device changes refresh it right away; this is the slow safety
        // net for the status file's "processing" flag.
        if self.state_checked.elapsed() >= Duration::from_secs(5) {
            self.refresh_install_state();
        }
        let live_age_ms = self.live.read().map(|levels| levels.age_ms());
        self.poll_health(live_age_ms);
        if self
            .window
            .as_mut()
            .is_some_and(|(child, _)| !matches!(child.try_wait(), Ok(None)))
        {
            self.window = None;
        }
        self.sync_tray();
        self.publish();
    }
}

/// Runs the agent until Quit. `open` shows the window at start (on `page`).
pub fn run(open: bool, page: Option<String>) {
    let Some(wake_event) = ipc::Event::unnamed().map(Arc::new) else {
        return;
    };
    let wake = {
        let event = wake_event.clone();
        move || event.set()
    };
    let events: Vec<ipc::Event> = [
        ipc::EV_SHOW,
        ipc::EV_REPAIR,
        ipc::EV_RESTART,
        ipc::EV_QUIT,
        ipc::EV_PREFS,
    ]
    .iter()
    .filter_map(|name| ipc::Event::create(name))
    .collect();
    if events.len() != 5 {
        return;
    }
    let mut agent = Agent::new(wake);
    agent.tick();
    // Without a tray icon the window is the only way in.
    if open || agent.tray.is_none() {
        agent.open_window(page.as_deref());
    }
    let mut handles: Vec<HANDLE> = vec![wake_event.0];
    handles.extend(events.iter().map(|e| e.0));
    while !agent.quitting {
        let result = unsafe {
            MsgWaitForMultipleObjects(Some(&handles), false, 1000, QS_ALLINPUT)
        };
        match result.0.wrapping_sub(WAIT_OBJECT_0.0) {
            1 => agent.open_window(ipc::take_page_request().as_deref()),
            2 => agent.start_repair(health::ActionKind::Repair, true, true),
            3 => agent.start_repair(health::ActionKind::Restart, true, true),
            4 => agent.quitting = true,
            5 => agent.reload_prefs(),
            _ => {}
        }
        // The tray icon's window lives on this thread.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    agent.quitting = true;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        agent.tick();
    }
    ipc::signal(ipc::EV_WINDOW_CLOSE);
    let _ = std::fs::remove_file(ipc::state_path());
}
