//! The Hush window: sidebar navigation with Microphone, Noise & Gate,
//! Voice, Mic Test, Settings and About pages.
//!
//! It runs in a process of its own (`hush.exe --window`) that the tray
//! agent starts on demand; closing the window ends the process, so no
//! graphics stay loaded while Hush sits in the tray. Health, repairs and
//! the mute hotkey belong to the agent; the window reads the agent's state
//! and asks it to act (see `ipc`).
//!
//! `mod.rs` holds the app state and the update loop; the sidebar, header,
//! banner and toast are in `chrome`, and each page in `pages/`.

mod chrome;
mod pages;

use crate::{
    dev, diagnostics, health, install_state, instance, ipc, level_watch, mic_test, mute, prefs,
    settings_io,
    theme::{self, palette},
    updates,
};
use eframe::egui::{self, ViewportCommand};
use hush_apo::{
    dsp::MicSettings,
    settings::{self},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

/// The tray agent asked the window to close (Hush is quitting).
static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// How often the window re-reads the agent's state and mic.conf.
const SYNC_EVERY: Duration = Duration::from_millis(400);
/// Frame time while meters move (20 fps).
const METER_FRAME: Duration = Duration::from_millis(50);

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

    fn from_id(id: &str) -> Option<Page> {
        Some(match id {
            "microphone" => Page::Microphone,
            "noise" => Page::Noise,
            "voice" => Page::Voice,
            "test" => Page::Test,
            "settings" => Page::Settings,
            "about" => Page::About,
            _ => return None,
        })
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

/// Microphone health, as the tray agent reports it.
struct HealthState {
    agent: ipc::AgentState,
    agent_running: bool,
    /// "Fix now" run by the window itself, when no agent is running.
    local_job: Option<(health::ActionKind, mpsc::Receiver<health::Outcome>)>,
    local_note: Option<String>,
}

impl HealthState {
    fn current(&self) -> &health::Health {
        &self.agent.health
    }

    fn repairing(&self) -> bool {
        self.agent.repairing || self.local_job.is_some()
    }

    fn note(&self) -> Option<&str> {
        self.local_note
            .as_deref()
            .or(self.agent.repair_note.as_deref())
    }

    fn target(&self) -> &str {
        &self.agent.target
    }
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
    /// mic.conf's modification time as last read or written here.
    settings_mtime: Option<SystemTime>,
    saved_settings: MicSettings,
    dirty_since: Option<Instant>,
    save_error: Option<String>,
    prefs: prefs::Prefs,
    start_with_windows: bool,
    prefs_error: Option<String>,
    mics: Vec<crate::endpoints::Endpoint>,
    selected_mic: String,
    setup_clipboard_note: Option<&'static str>,
    state: install_state::InstallState,
    state_checked: Instant,
    synced: Instant,
    jobs: JobsState,
    test_state: TestState,
    live: settings::LiveReader,
    live_levels: Option<settings::LiveLevels>,
    compare: Option<(settings::CompareLease, Instant, Page)>,
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
    health: HealthState,
    /// Last known window position, saved to app.conf when it closes.
    window_rect: Option<prefs::WindowRect>,
}

/// The microphone Hush selects on start: the first one Clean Mic is
/// installed on, else the first one found.
fn default_mic(state: &install_state::InstallState, mics: &[crate::endpoints::Endpoint]) -> String {
    mics.iter()
        .find(|mic| state.capture_ids.contains(&mic.id))
        .or_else(|| mics.first())
        .map(|mic| mic.id.clone())
        .unwrap_or_default()
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether a saved window position is still on a screen (monitors change).
fn on_screen(rect: &prefs::WindowRect) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN) as f32,
            GetSystemMetrics(SM_YVIRTUALSCREEN) as f32,
            GetSystemMetrics(SM_CXVIRTUALSCREEN) as f32,
            GetSystemMetrics(SM_CYVIRTUALSCREEN) as f32,
        )
    };
    // The title bar must be reachable: check a point near its middle.
    let x = (rect.x + rect.width / 2.0) * rect.pixels_per_point;
    let y = (rect.y + 12.0) * rect.pixels_per_point;
    x >= left && x < left + width && y >= top && y < top + height
}

/// Runs the window until it closes. `page` picks the first page.
pub fn run(page: Option<String>) -> eframe::Result {
    let icon = egui::IconData {
        rgba: crate::icon_art::render_icon(64),
        width: 64,
        height: 64,
    };
    let saved = prefs::load().window.filter(on_screen);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Hush")
        .with_min_inner_size([680.0, 480.0])
        .with_icon(icon);
    let mut centered = true;
    match (dev::shot_path(), saved) {
        (None, Some(rect)) => {
            viewport = viewport
                .with_inner_size([rect.width, rect.height])
                .with_position([rect.x, rect.y]);
            centered = false;
        }
        _ => viewport = viewport.with_inner_size(dev::window_size()),
    }
    let options = eframe::NativeOptions {
        viewport,
        centered,
        ..Default::default()
    };
    eframe::run_native(
        "Hush",
        options,
        Box::new(move |cc| Ok(Box::new(HushApp::new(&cc.egui_ctx, page.as_deref())))),
    )
}

impl HushApp {
    pub fn new(ctx: &egui::Context, page: Option<&str>) -> Self {
        theme::install(ctx);
        ctx.add_plugin(crate::clipboard::SystemClipboard);
        if let Some(preference) = dev::theme() {
            ctx.set_theme(preference);
        }
        let wake_ctx = ctx.clone();
        let wake = move || wake_ctx.request_repaint();
        if dev::shot_path().is_none() {
            instance::listen(ipc::EV_WINDOW_SHOW, wake.clone());
            let close_ctx = ctx.clone();
            ipc::on_event(ipc::EV_WINDOW_CLOSE, move || {
                CLOSE_REQUESTED.store(true, Ordering::Release);
                close_ctx.request_repaint();
            });
        }

        let settings = settings::load();
        let state = install_state::read();
        let mics = crate::endpoints::physical_microphones();
        let selected_mic = default_mic(&state, &mics);
        let mute = mute::Mute::new(&selected_mic, wake);
        let pending = ipc::take_page_request();
        let page = dev::page()
            .as_deref()
            .or(pending.as_deref())
            .or(page)
            .and_then(Page::from_id)
            .unwrap_or(Page::Microphone);
        let mut app = Self {
            page,
            settings,
            saved_settings: settings,
            settings_mtime: mtime(&settings::settings_path()),
            dirty_since: None,
            save_error: None,
            prefs: prefs::load(),
            start_with_windows: prefs::start_with_windows(),
            prefs_error: None,
            mics,
            selected_mic,
            setup_clipboard_note: None,
            state,
            state_checked: Instant::now(),
            synced: Instant::now() - SYNC_EVERY,
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
            live: settings::LiveReader::new(),
            live_levels: None,
            compare: None,
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
            health: HealthState {
                agent: ipc::AgentState::default(),
                agent_running: false,
                local_job: None,
                local_note: None,
            },
            window_rect: None,
        };
        app.sync_with_agent();
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

    /// Saves app.conf and tells the agent (hotkey, notifications, repair).
    fn save_prefs(&mut self) {
        self.prefs_error = prefs::save(&self.prefs)
            .err()
            .map(|e| format!("Could not save: {e}"));
        ipc::signal(ipc::EV_PREFS);
    }

    /// Rescans microphones and Clean Mic installs. Keeps the selection when
    /// that microphone is still there.
    fn refresh_mics(&mut self) {
        self.mics = crate::endpoints::physical_microphones();
        self.state.refresh();
        self.state_checked = Instant::now();
        if !self.mics.iter().any(|mic| mic.id == self.selected_mic) {
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

    fn copy_diagnostics(&mut self) {
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
        let health = if self.health.agent_running {
            format!("{:?}", self.health.current())
        } else {
            "unknown (tray agent not running)".into()
        };
        let report = diagnostics::Report {
            version: env!("CARGO_PKG_VERSION"),
            windows: &windows,
            selected_mic: self.selected_mic_name().unwrap_or(""),
            attached: &self.state.attached,
            processing: self.state.running,
            status: status.as_ref(),
            now_unix,
            glitches: self.state.glitches,
            settings: &self.settings,
            mute_hotkey: self.prefs.mute_hotkey.then(|| self.prefs.mute_key.label()),
            muted: self.mute.is_muted(),
            health: &health,
            last_repair: last_repair.as_deref(),
            repair_tasks: &repair_tasks,
        };
        if crate::clipboard::copy(&diagnostics::text(&report)) {
            self.diag_copied = Some(Instant::now());
        }
    }

    /// "Fix now": the agent runs it (and may ask for administrator rights);
    /// without an agent the window runs it itself.
    fn fix_now(&mut self, kind: health::ActionKind) {
        if self.health.repairing() {
            return;
        }
        self.health.local_note = None;
        self.health.local_job = Some((kind, health::run_repair(kind, true)));
    }

    /// Re-reads the agent's state and picks up settings changed elsewhere
    /// (the tray's Clean Mic switch).
    fn sync_with_agent(&mut self) {
        self.synced = Instant::now();
        self.health.agent_running = ipc::agent_running();
        let agent = if self.health.agent_running {
            ipc::AgentState::read().unwrap_or_else(|| self.health.agent.clone())
        } else {
            ipc::AgentState::default()
        };
        if agent.repairing != self.health.agent.repairing && !agent.repairing {
            // A repair just finished: show what changed.
            self.state.refresh();
            self.state_checked = Instant::now();
        }
        self.health.agent = agent;
        let settings_mtime = mtime(&settings::settings_path());
        if settings_mtime != self.settings_mtime && self.dirty_since.is_none() {
            if let Ok(loaded) = settings::try_load() {
                self.settings_mtime = settings_mtime;
                self.settings = loaded;
                self.saved_settings = loaded;
            }
        }
    }

    fn poll_local_repair(&mut self) {
        let finished = match &self.health.local_job {
            Some((_, job)) => match job.try_recv() {
                Ok(outcome) => Some(Some(outcome)),
                Err(mpsc::TryRecvError::Disconnected) => Some(None),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        if let Some(outcome) = finished {
            self.health.local_job = None;
            if !outcome.as_ref().is_some_and(|o| o.success) {
                self.health.local_note = Some(
                    outcome
                        .map(|o| o.message)
                        .unwrap_or_else(|| "The repair did not finish.".into()),
                );
            }
            self.state.refresh();
            self.state_checked = Instant::now();
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

    /// Commits only the settings the user changed.
    fn save_now(&mut self) {
        let before = self.saved_settings;
        let after = self.settings;
        match settings::update(|_| (before, after)) {
            Ok(saved) => {
                self.dirty_since = None;
                self.saved_settings = saved;
                self.settings = saved;
                self.save_error = None;
                self.settings_mtime = mtime(&settings::settings_path());
            }
            Err(error) => {
                self.dirty_since = Some(Instant::now());
                self.save_error = Some(format!("Could not save settings: {error}"));
            }
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
    }

    fn start_test(&mut self) {
        if self.selected_mic.is_empty() {
            self.test_state.error = Some("Choose a connected microphone first.".into());
            return;
        }
        match mic_test::MicTest::start(&self.selected_mic) {
            Ok(test) => {
                self.test_state.error = None;
                if let Err(error) = test.set_monitor(self.test_state.monitor) {
                    self.test_state.monitor = false;
                    self.test_state.error = Some(format!("{error:#}"));
                }
                self.test_state.mic = Some(test);
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
            let result = match job.try_recv() {
                Ok(message) => Some(message),
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some("Setup stopped before reporting a result.".into())
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(message) = result {
                self.jobs.setup_message = Some(message);
                self.jobs.setup = None;
                self.refresh_mics();
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
        // Slow safety net for the status file's "processing" flag.
        if self.state_checked.elapsed() >= Duration::from_secs(2) {
            self.state.refresh();
            self.state_checked = Instant::now();
        }
        if let Some(test) = &mut self.test_state.mic {
            if let Some(error) = test.poll_error() {
                self.test_state.monitor = false;
                self.test_state.error = Some(error);
            }
            let peak = if test.is_running() { test.peak() } else { 0.0 };
            self.test_state.meter = if peak > self.test_state.meter {
                peak
            } else {
                self.test_state.meter * 0.85 + peak * 0.15
            };
        }
    }

    fn selected_mic_name(&self) -> Option<&str> {
        self.mics
            .iter()
            .find(|mic| mic.id == self.selected_mic)
            .map(|mic| mic.name.as_str())
    }

    fn attached_here(&self) -> bool {
        self.state.capture_ids.contains(&self.selected_mic)
    }

    fn status(&self) -> (egui::Color32, &'static str) {
        let p = if self.ctx.theme() == egui::Theme::Dark {
            theme::DARK
        } else {
            theme::LIGHT
        };
        if !self.attached_here() {
            (egui::Color32::GRAY, "Not installed")
        } else if !self.settings.enabled {
            (egui::Color32::GRAY, "Off")
        } else if self.state.degraded {
            (p.warning, "Processing degraded")
        } else if self.state.running {
            (p.success, "Active")
        } else {
            (p.warning, "Ready (mic idle)")
        }
    }

    /// Apps using the microphone right now, for the header ("Discord").
    fn apps_label(&self) -> Option<String> {
        let apps = &self.health.agent.apps;
        let watched = self.health.agent.target_id == self.selected_mic;
        if !watched || apps.is_empty() {
            return None;
        }
        Some(match apps.len() {
            1 | 2 => apps.join(", "),
            n => format!("{}, {} +{}", apps[0], apps[1], n - 2),
        })
    }

    /// Remembers where the window is, so it reopens there.
    fn track_window(&mut self, ctx: &egui::Context) {
        let rect = ctx.input(|i| {
            let viewport = i.viewport();
            match (viewport.outer_rect, viewport.inner_rect, viewport.minimized) {
                (Some(outer), Some(inner), Some(false) | None) => Some(prefs::WindowRect {
                    x: outer.min.x,
                    y: outer.min.y,
                    width: inner.width(),
                    height: inner.height(),
                    pixels_per_point: i.pixels_per_point,
                }),
                _ => None,
            }
        });
        if rect.is_some() {
            self.window_rect = rect;
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
        if instance::SHOW_REQUESTED.swap(false, Ordering::AcqRel) {
            if let Some(page) = ipc::take_page_request().as_deref().and_then(Page::from_id) {
                self.page = page;
            }
            self.show_window(ctx);
        }
        if CLOSE_REQUESTED.swap(false, Ordering::AcqRel) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        self.track_window(ctx);
        let still_holding = ctx.input(|input| {
            input.focused
                && !input.viewport().close_requested()
                && (input.pointer.primary_down()
                    || input.key_down(egui::Key::Space)
                    || input.key_down(egui::Key::Enter))
        });
        if !still_holding
            || self
                .compare
                .as_ref()
                .is_some_and(|(_, _, page)| self.page != *page)
        {
            self.compare = None;
        } else if let Some((lease, renewed, _)) = &mut self.compare {
            if renewed.elapsed() >= Duration::from_secs(1) {
                if lease.renew().is_ok() {
                    *renewed = Instant::now();
                } else {
                    self.compare = None;
                }
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.prefs.close_to_tray {
            // "Keep running in the tray" is off: closing quits Hush.
            ipc::signal(ipc::EV_QUIT);
        }

        self.poll_background();
        self.poll_local_repair();
        if self.synced.elapsed() >= SYNC_EVERY {
            self.sync_with_agent();
        }
        self.live_levels = self.live.read();
        let fresh = self.live_levels.filter(|l| l.age_ms() < 1500);
        self.level_warning = self.level_watch.update(fresh, Instant::now());
        self.save_if_needed();
        if self
            .diag_copied
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
        {
            self.diag_copied = None;
        }

        let busy =
            self.jobs.setup.is_some() || self.jobs.update.is_some() || self.health.repairing();
        // Meters are drawn at 20 fps (the plugin's own update rate), and
        // only on the pages that show them; elsewhere live audio only moves
        // the "Talking" badge.
        let live = self.live_levels.is_some_and(|l| l.age_ms() < 1500);
        let meters = (live && self.page == Page::Noise)
            || (self
                .test_state
                .mic
                .as_ref()
                .is_some_and(mic_test::MicTest::is_running)
                && self.page == Page::Test);
        if meters || self.compare.is_some() {
            ctx.request_repaint_after(METER_FRAME);
        } else if self.dirty_since.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if live || busy {
            ctx.request_repaint_after(Duration::from_millis(250));
        } else {
            ctx.request_repaint_after(SYNC_EVERY);
        }
        if let Some(copied) = self.diag_copied {
            ctx.request_repaint_after(Duration::from_secs(2).saturating_sub(copied.elapsed()));
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
        self.compare = None;
        if self.dirty_since.is_some() {
            self.save_now();
        }
        if self.shot.is_none() {
            if let Some(rect) = self.window_rect {
                // Only the position changes; re-read so nothing else is undone.
                let mut current = prefs::load();
                current.window = Some(rect);
                let _ = prefs::save(&current);
            }
        }
    }
}

/// Opens a file or web page with its default Windows program.
pub(in crate::ui) fn open_in_shell(target: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn();
}
