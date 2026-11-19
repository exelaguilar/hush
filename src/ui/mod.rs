//! The Hush window: sidebar navigation with Microphone, Noise & Gate,
//! Voice, Mic Test, Settings and About pages. Lives in the system tray;
//! closing the window hides it (configurable).
//!
//! `mod.rs` holds the app state and the update loop; the sidebar, header,
//! banner and toast are in `chrome`, and each page in `pages/`.

mod chrome;
mod pages;

use crate::{
    dev, diagnostics, health, install_state, instance, level_watch, mic_test, mute, notify, prefs,
    settings_io,
    theme::{self, palette},
    tray::{Tray, TrayAction},
    updates,
};
use eframe::egui::{self, ViewportCommand};
use hush_apo::{
    dsp::MicSettings,
    settings::{self},
};
use std::{
    sync::{atomic::Ordering, mpsc},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Microphone,
    Noise,
    Voice,
    Test,
    Settings,
    About,
}

impl Page {
    const MAIN: [Page; 4] = [Page::Microphone, Page::Noise, Page::Voice, Page::Test];
    const FOOTER: [Page; 2] = [Page::Settings, Page::About];

    fn label(self) -> &'static str {
        match self {
            Page::Microphone => "Microphone",
            Page::Noise => "Noise & Gate",
            Page::Voice => "Voice",
            Page::Test => "Mic Test",
            Page::Settings => "Settings",
            Page::About => "About",
        }
    }
}

/// Mic Test page: the running test and what came out of it.
struct TestState {
    mic: Option<mic_test::MicTest>,
    error: Option<String>,
    /// Result of the last "Save recording" (path or error).
    saved: Option<(Result<std::path::PathBuf, String>, Instant)>,
    monitor: bool,
    meter: f32,
}

/// Microphone health watch and repair.
struct HealthState {
    watch: health::Watcher,
    monitor: health::Monitor,
    current: health::Health,
    /// Latest probe of the microphone (None until the watcher reports).
    probe: Option<health::Probe>,
    target: String,
    repair_job: Option<(health::ActionKind, mpsc::Receiver<health::Outcome>)>,
    /// Message of the last failed repair, shown in the banner.
    repair_note: Option<String>,
}

/// Background jobs started from the window.
struct JobsState {
    setup: Option<mpsc::Receiver<String>>,
    setup_message: Option<String>,
    file: Option<mpsc::Receiver<settings_io::DialogResult>>,
    update: Option<mpsc::Receiver<Result<updates::Outcome, String>>>,
    update_result: Option<Result<updates::Outcome, String>>,
}

pub struct HushApp {
    page: Page,
    settings: MicSettings,
    dirty_since: Option<Instant>,
    save_error: Option<String>,
    prefs: prefs::Prefs,
    start_with_windows: bool,
    prefs_error: Option<String>,
    mics: Vec<String>,
    selected_mic: String,
    state: install_state::InstallState,
    state_checked: Instant,
    jobs: JobsState,
    test_state: TestState,
    tray: Option<Tray>,
    visible: bool,
    quitting: bool,
    live: settings::LiveReader,
    live_levels: Option<settings::LiveLevels>,
    comparing: bool,
    /// "Reset to defaults" was clicked once; a second click within a few
    /// seconds confirms it.
    reset_armed: Option<Instant>,
    logo: Option<egui::TextureHandle>,
    /// Developer aid (debug builds): HUSH_SHOT=<file.png> saves a screenshot and exits.
    shot: Option<(std::path::PathBuf, Instant, bool)>,
    ctx: egui::Context,
    level_watch: level_watch::LevelWatch,
    level_warning: Option<level_watch::LevelWarning>,
    /// "Copy diagnostics" was clicked; the label says "Copied" for a moment.
    diag_copied: Option<Instant>,
    /// When the tuning snapshot was saved, if there is one.
    snapshot_saved: Option<SystemTime>,
    /// Result line under the tuning / export / import rows (text, success).
    tuning_message: Option<(String, bool)>,
    mute: mute::Mute,
    hotkey: Option<mute::Hotkey>,
    hotkey_error: Option<String>,
    health: HealthState,
}

/// The microphone whose health is watched: the selected one if Clean Mic is
/// installed on it, else the first one it is installed on.
fn health_target(state: &install_state::InstallState, selected: &str) -> String {
    if state.attached.iter().any(|name| name == selected) {
        selected.to_string()
    } else {
        state.attached.first().cloned().unwrap_or_default()
    }
}

/// The microphone Hush selects on start: the first one Clean Mic is
/// installed on, else the first one found.
fn default_mic(state: &install_state::InstallState, mics: &[String]) -> String {
    state
        .attached
        .first()
        .filter(|name| mics.contains(name))
        .cloned()
        .or_else(|| mics.first().cloned())
        .unwrap_or_default()
}

impl HushApp {
    pub fn new(ctx: &egui::Context, start_hidden: bool) -> Self {
        theme::install(ctx);
        if let Some(preference) = dev::theme() {
            ctx.set_theme(preference);
        }
        let wake_ctx = ctx.clone();
        let wake = move || wake_ctx.request_repaint();
        instance::listen(wake.clone());

        let settings = settings::load();
        let state = install_state::read();
        let mics = mic_test::microphones();
        let selected_mic = default_mic(&state, &mics);
        let mute = mute::Mute::new(&selected_mic, wake.clone());
        let target = health_target(&state, &selected_mic);
        let health_watch = health::Watcher::new(&target, wake.clone());
        // Toast identity (name + icon) for health notifications.
        std::thread::spawn(notify::register);
        let tray = Tray::new(settings.enabled, false, wake);
        let page = match dev::page().as_deref() {
            Some("noise") => Page::Noise,
            Some("voice") => Page::Voice,
            Some("test") => Page::Test,
            Some("settings") => Page::Settings,
            Some("about") => Page::About,
            _ => Page::Microphone,
        };
        let mut app = Self {
            page,
            settings,
            dirty_since: None,
            save_error: None,
            prefs: prefs::load(),
            start_with_windows: prefs::start_with_windows(),
            prefs_error: None,
            mics,
            selected_mic,
            state,
            state_checked: Instant::now(),
            jobs: JobsState {
                setup: None,
                setup_message: None,
                file: None,
                update: None,
                update_result: None,
            },
            test_state: TestState {
                mic: None,
                error: None,
                saved: dev::demo_toast().then(|| {
                    (
                        Ok(mic_test::recordings_dir().join("hush-example.wav")),
                        Instant::now(),
                    )
                }),
                monitor: false,
                meter: 0.0,
            },
            // Without a tray icon, a hidden window could never be reopened.
            visible: !start_hidden || tray.is_none(),
            tray,
            quitting: false,
            live: settings::LiveReader::new(),
            live_levels: None,
            comparing: false,
            reset_armed: None,
            logo: None,
            shot: dev::shot_path().map(|path| (path, Instant::now(), false)),
            ctx: ctx.clone(),
            level_watch: level_watch::LevelWatch::default(),
            level_warning: None,
            diag_copied: None,
            snapshot_saved: settings_io::snapshot_time(),
            tuning_message: None,
            mute,
            hotkey: None,
            hotkey_error: None,
            health: HealthState {
                watch: health_watch,
                monitor: health::Monitor::default(),
                current: health::Health::Healthy,
                probe: None,
                target,
                repair_job: None,
                repair_note: None,
            },
        };
        app.apply_hotkey();
        if app.shot.is_some() {
            if let Some(scale) = dev::shot_scale() {
                ctx.set_zoom_factor(scale);
            }
            if dev::shot_test() {
                app.start_test();
            }
        }
        app
    }

    fn wake(&self) -> impl Fn() + Send + 'static {
        let ctx = self.ctx.clone();
        move || ctx.request_repaint()
    }

    /// (Re)registers the global mute hotkey to match the preferences.
    fn apply_hotkey(&mut self) {
        // Unregister first so a changed key never leaves the old one active.
        self.hotkey = None;
        self.hotkey_error = None;
        if self.prefs.mute_hotkey {
            match mute::Hotkey::register(self.prefs.mute_key, self.mute.toggler()) {
                Ok(hotkey) => self.hotkey = Some(hotkey),
                Err(error) => self.hotkey_error = Some(error),
            }
        }
    }

    fn save_prefs(&mut self) {
        self.prefs_error = prefs::save(&self.prefs)
            .err()
            .map(|e| format!("Could not save: {e}"));
    }

    /// Rescans microphones and Clean Mic installs. Keeps the selection when
    /// that microphone is still there.
    fn refresh_mics(&mut self) {
        self.mics = mic_test::microphones();
        self.state = install_state::read();
        self.state_checked = Instant::now();
        if !self.mics.contains(&self.selected_mic) {
            self.selected_mic = default_mic(&self.state, &self.mics);
            self.mute.set_device(&self.selected_mic);
            // The microphone being tested is gone.
            self.test_state.mic = None;
            self.test_state.monitor = false;
        }
    }

    /// Applies settings from a snapshot or file. Clean Mic on/off stays as it is.
    fn apply_loaded_settings(&mut self, mut loaded: MicSettings) {
        loaded.enabled = self.settings.enabled;
        self.settings = loaded;
        self.mark_dirty();
    }

    fn copy_diagnostics(&mut self, ctx: &egui::Context) {
        let status = std::fs::read_to_string(settings::status_path())
            .ok()
            .and_then(|text| settings::parse_status(&text));
        let now_unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let windows = diagnostics::windows_version();
        let last_repair = health::read_repair_file().map(|file| {
            format!(
                "{}: {} ({} s ago)",
                file.result,
                file.message,
                now_unix.saturating_sub(file.time_unix)
            )
        });
        // Only queried here, when the user asks for diagnostics.
        let (repair_task, restart_task) = health::repair_tasks_exist();
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        let repair_tasks = format!(
            "repair {}, restart {}",
            yes_no(repair_task),
            yes_no(restart_task)
        );
        let report = diagnostics::Report {
            version: env!("CARGO_PKG_VERSION"),
            windows: &windows,
            selected_mic: &self.selected_mic,
            attached: &self.state.attached,
            processing: self.state.running,
            status: status.as_ref(),
            now_unix,
            glitches: self.state.glitches,
            settings: &self.settings,
            mute_hotkey: self.prefs.mute_hotkey.then(|| self.prefs.mute_key.label()),
            muted: self.mute.is_muted(),
            health: &format!("{:?}", self.health.current),
            last_repair: last_repair.as_deref(),
            repair_tasks: &repair_tasks,
        };
        let _ = ctx;
        if crate::clipboard::copy(&diagnostics::text(&report)) {
            self.diag_copied = Some(Instant::now());
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

    /// Watches the microphone: device changes, repairs, notifications.
    fn poll_health(&mut self) {
        let now = Instant::now();
        let target = health_target(&self.state, &self.selected_mic);
        if target != self.health.target {
            self.health.watch.set_target(&target);
            self.health.target = target;
        }
        if let Some(update) = self.health.watch.poll() {
            self.health.probe = Some(update.probe);
            if update.devices_changed {
                // Keeps the device picker from going stale.
                self.refresh_mics();
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
            self.state = install_state::read();
            self.state_checked = Instant::now();
        }
        let Some(probe) = self.health.probe.clone() else {
            return;
        };
        let snapshot = health::Snapshot {
            mic: self.health.target.clone(),
            installed: !self.state.attached.is_empty(),
            clean_mic_on: self.settings.enabled,
            probe,
            live_age_ms: self.live_levels.map(|levels| levels.age_ms()),
        };
        let config = health::Config {
            auto_repair: self.prefs.auto_repair,
            hold_pressed: self.comparing,
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

    fn mark_dirty(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    fn save_if_needed(&mut self) {
        if let Some(since) = self.dirty_since {
            if since.elapsed() >= Duration::from_millis(100) {
                self.save_now();
            }
        }
    }

    /// Saves immediately. While "hold to hear original" is pressed the
    /// processing is bypassed without changing the visible setting.
    fn save_now(&mut self) {
        self.dirty_since = None;
        let mut effective = self.settings;
        effective.enabled = self.settings.enabled && !self.comparing;
        self.save_error = settings::save(&effective)
            .err()
            .map(|e| format!("Could not save settings: {e}"));
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.visible = true;
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.visible = false;
        // Release the microphone while nobody is looking at the test.
        self.test_state.mic = None;
        self.test_state.monitor = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    fn start_test(&mut self) {
        match mic_test::MicTest::start(&self.selected_mic) {
            Ok(test) => {
                test.set_monitor(self.test_state.monitor);
                self.test_state.mic = Some(test);
                self.test_state.error = None;
            }
            Err(error) => self.test_state.error = Some(format!("{error:#}")),
        }
    }

    fn run_setup(&mut self, args: Vec<String>) {
        self.test_state.mic = None;
        self.test_state.monitor = false;
        self.jobs.setup = Some(install_state::run_setup(args));
        self.jobs.setup_message = Some("Waiting for Windows permission…".into());
    }

    fn poll_background(&mut self) {
        if let Some(job) = &self.jobs.setup {
            if let Ok(message) = job.try_recv() {
                self.jobs.setup_message = Some(message);
                self.jobs.setup = None;
                self.state = install_state::read();
                self.state_checked = Instant::now();
            }
        }
        if let Some(job) = &self.jobs.file {
            match job.try_recv() {
                Ok(settings_io::DialogResult::Exported(path)) => {
                    self.tuning_message = Some((format!("Exported to {}.", path.display()), true));
                    self.jobs.file = None;
                }
                Ok(settings_io::DialogResult::Imported(imported)) => {
                    self.apply_loaded_settings(imported);
                    self.tuning_message = Some((
                        "Imported. Clean Mic stays as it was (on or off).".into(),
                        true,
                    ));
                    self.jobs.file = None;
                }
                Ok(settings_io::DialogResult::Failed(message)) => {
                    self.tuning_message = Some((message, false));
                    self.jobs.file = None;
                }
                // The dialog was cancelled.
                Err(mpsc::TryRecvError::Disconnected) => self.jobs.file = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(job) = &self.jobs.update {
            match job.try_recv() {
                Ok(result) => {
                    self.jobs.update_result = Some(result);
                    self.jobs.update = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.jobs.update_result = Some(Err("The update check did not finish.".into()));
                    self.jobs.update = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        // Device changes refresh it right away (refresh_mics); this is the
        // slow safety net for the status file's "processing" flag.
        if self.state_checked.elapsed() >= Duration::from_secs(5) {
            self.state = install_state::read();
            self.state_checked = Instant::now();
        }
        if let Some(test) = &self.test_state.mic {
            let peak = test.peak();
            self.test_state.meter = if peak > self.test_state.meter {
                peak
            } else {
                self.test_state.meter * 0.85 + peak * 0.15
            };
        }
    }

    fn attached_here(&self) -> bool {
        self.state.attached.contains(&self.selected_mic)
    }

    fn status(&self) -> (egui::Color32, &'static str) {
        let p = theme::DARK;
        if !self.attached_here() {
            (egui::Color32::GRAY, "Not installed")
        } else if !self.settings.enabled {
            (egui::Color32::GRAY, "Off")
        } else if self.state.running {
            (p.success, "Active")
        } else {
            (p.warning, "Ready (mic idle)")
        }
    }
}

impl HushApp {
    fn developer_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, started, requested)) = &mut self.shot else {
            return;
        };
        let delay = dev::shot_delay_ms();
        if !*requested && started.elapsed() > Duration::from_millis(delay) {
            *requested = true;
            let path = path.clone();
            ctx.request_screenshot(move |image| {
                let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
                let png =
                    crate::notify::encode_png(image.size[0] as u32, image.size[1] as u32, &rgba);
                let _ = std::fs::write(path, png);
                std::process::exit(0);
            });
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

impl eframe::App for HushApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.developer_screenshot(ctx);
        let actions: Vec<TrayAction> = match &self.tray {
            Some(tray) => std::iter::from_fn(|| tray.poll()).collect(),
            None => Vec::new(),
        };
        {
            for action in actions {
                match action {
                    TrayAction::Open => self.show_window(ctx),
                    TrayAction::OpenMicTest => {
                        // Only opens the page; the user starts the test.
                        self.page = Page::Test;
                        self.show_window(ctx);
                    }
                    TrayAction::ToggleCleanMic => {
                        self.settings.enabled = !self.settings.enabled;
                        self.mark_dirty();
                    }
                    TrayAction::ToggleMute => self.mute.toggle(),
                    TrayAction::Quit => {
                        self.quitting = true;
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                }
            }
        }
        if instance::SHOW_REQUESTED.swap(false, Ordering::AcqRel) {
            self.show_window(ctx);
        }
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested && !self.quitting && self.prefs.close_to_tray && self.tray.is_some() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }

        self.poll_background();
        // Read even while hidden: the health watch needs to know whether the
        // plugin is processing.
        self.live_levels = self.live.read();
        self.poll_health();
        if self.visible {
            let fresh = self.live_levels.filter(|l| l.age_ms() < 1500);
            self.level_warning = self.level_watch.update(fresh, Instant::now());
        }
        self.save_if_needed();
        let muted = self.mute.is_muted();
        if let Some(tray) = &self.tray {
            tray.sync(
                self.settings.enabled,
                muted,
                self.health.current.needs_attention(),
            );
            if self.health.current.needs_attention() && !muted {
                tray.set_tooltip(&format!("Hush – {}", self.health.current.short()));
            } else if muted {
                tray.set_tooltip("Hush \u{2013} microphone muted");
            } else {
                let (_, status) = self.status();
                tray.set_tooltip(&format!("Hush - Clean Mic: {status}"));
            }
        }
        if self
            .diag_copied
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
        {
            self.diag_copied = None;
        }

        let busy = self.dirty_since.is_some()
            || self.jobs.setup.is_some()
            || self.jobs.update.is_some()
            || self.diag_copied.is_some();
        let live = self.live_levels.is_some_and(|l| l.age_ms() < 1500);
        if self.visible && (self.test_state.mic.is_some() || busy || live || self.comparing) {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = palette(ui);
        egui::Panel::left("nav")
            .exact_size(190.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(p.sidebar)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                let rect = ui.max_rect();
                let x = ui.clip_rect().right() - 0.5;
                ui.painter().vline(
                    x,
                    rect.top() - 100.0..=rect.bottom() + 100.0,
                    egui::Stroke::new(1.0, p.border),
                );
                self.sidebar(ui);
            });
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(p.background)
                    .inner_margin(egui::Margin::symmetric(28, 6)),
            )
            .show(ui, |ui| self.header(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.background)
                    .inner_margin(egui::Margin {
                        left: 28,
                        right: 28,
                        top: 8,
                        bottom: 20,
                    }),
            )
            .show(ui, |ui| {
                if self.page != Page::Settings {
                    self.reset_armed = None;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let width = (ui.available_width() - 14.0).clamp(0.0, 760.0);
                    ui.set_max_width(width);
                    match self.page {
                        Page::Microphone => self.microphone_page(ui),
                        Page::Noise => self.noise_page(ui),
                        Page::Voice => self.voice_page(ui),
                        Page::Test => self.test_page(ui),
                        Page::Settings => self.settings_page(ui),
                        Page::About => self.about_page(ui),
                    }
                    ui.add_space(8.0);
                });
            });
        let ctx = ui.ctx().clone();
        self.recording_toast(&ctx);
    }
}

impl Drop for HushApp {
    fn drop(&mut self) {
        if self.dirty_since.is_some() {
            let _ = settings::save(&self.settings);
        }
    }
}

/// Opens a file or web page with its default Windows program.
pub(in crate::ui) fn open_in_shell(target: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn();
}
