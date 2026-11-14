//! The Hush window: sidebar navigation with Microphone, Noise & Gate,
//! Voice, Mic Test, Settings and About pages. Lives in the system tray;
//! closing the window hides it (configurable).

use crate::{
    diagnostics, health, install_state, instance, level_watch, mic_test, mute, notify, prefs,
    settings_io,
    theme::{self, palette},
    tray::{Tray, TrayAction},
    updates,
};
use eframe::egui::{self, RichText, ViewportCommand};
use hush_apo::{
    dsp::MicSettings,
    settings::{self, Preset},
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
    setup_job: Option<mpsc::Receiver<String>>,
    setup_message: Option<String>,
    test: Option<mic_test::MicTest>,
    test_error: Option<String>,
    /// Result of the last "Save recording" (path or error).
    saved_recording: Option<(Result<std::path::PathBuf, String>, Instant)>,
    monitor: bool,
    meter: f32,
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
    /// Developer aid: HUSH_SHOT=<file.png> saves a screenshot and exits.
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
    file_job: Option<mpsc::Receiver<settings_io::DialogResult>>,
    mute: mute::Mute,
    hotkey: Option<mute::Hotkey>,
    hotkey_error: Option<String>,
    update_job: Option<mpsc::Receiver<Result<updates::Outcome, String>>>,
    update_result: Option<Result<updates::Outcome, String>>,
    health_watch: health::Watcher,
    health_monitor: health::Monitor,
    health: health::Health,
    /// Latest probe of the microphone (None until the watcher reports).
    health_probe: Option<health::Probe>,
    health_target: String,
    repair_job: Option<(health::ActionKind, mpsc::Receiver<health::Outcome>)>,
    /// Message of the last failed repair, shown in the banner.
    repair_note: Option<String>,
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
        // Developer aid: HUSH_PAGE=voice opens a specific page.
        let page = match std::env::var("HUSH_PAGE").as_deref() {
            Ok("noise") => Page::Noise,
            Ok("voice") => Page::Voice,
            Ok("test") => Page::Test,
            Ok("settings") => Page::Settings,
            Ok("about") => Page::About,
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
            setup_job: None,
            setup_message: None,
            test: None,
            test_error: None,
            // Developer aid for screenshots: HUSH_DEMO_TOAST=1.
            saved_recording: std::env::var_os("HUSH_DEMO_TOAST").map(|_| {
                (
                    Ok(mic_test::recordings_dir().join("hush-example.wav")),
                    Instant::now(),
                )
            }),
            monitor: false,
            meter: 0.0,
            // Without a tray icon, a hidden window could never be reopened.
            visible: !start_hidden || tray.is_none(),
            tray,
            quitting: false,
            live: settings::LiveReader::new(),
            live_levels: None,
            comparing: false,
            reset_armed: None,
            logo: None,
            shot: std::env::var_os("HUSH_SHOT")
                .map(|path| (path.into(), Instant::now(), false)),
            ctx: ctx.clone(),
            level_watch: level_watch::LevelWatch::default(),
            level_warning: None,
            diag_copied: None,
            snapshot_saved: settings_io::snapshot_time(),
            tuning_message: None,
            file_job: None,
            mute,
            hotkey: None,
            hotkey_error: None,
            update_job: None,
            update_result: None,
            health_watch,
            health_monitor: health::Monitor::default(),
            health: health::Health::Healthy,
            health_probe: None,
            health_target: target,
            repair_job: None,
            repair_note: None,
        };
        app.apply_hotkey();
        if app.shot.is_some() {
            // README screenshots: HUSH_SHOT_SCALE renders at a higher DPI;
            // HUSH_SHOT_TEST=1 opens the mic so the meters move.
            if let Some(scale) = std::env::var("HUSH_SHOT_SCALE")
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
            {
                ctx.set_zoom_factor(scale);
            }
            if std::env::var_os("HUSH_SHOT_TEST").is_some() {
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
            self.test = None;
            self.monitor = false;
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
            health: &format!("{:?}", self.health),
            last_repair: last_repair.as_deref(),
            repair_tasks: &repair_tasks,
        };
        let _ = ctx;
        if crate::clipboard::copy(&diagnostics::text(&report)) {
            self.diag_copied = Some(Instant::now());
        }
    }

    fn diagnostics_button(&mut self, ui: &mut egui::Ui) {
        let copied = self
            .diag_copied
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        let label = if copied { "Copied" } else { "Copy diagnostics" };
        if theme::outline_button(ui, label).clicked() {
            let ctx = ui.ctx().clone();
            self.copy_diagnostics(&ctx);
        }
    }

    fn start_repair(&mut self, kind: health::ActionKind, allow_uac: bool, manual: bool) {
        if self.repair_job.is_some() {
            return;
        }
        if manual {
            self.health_monitor.manual_started();
        }
        self.repair_note = None;
        self.repair_job = Some((kind, health::run_repair(kind, allow_uac)));
    }

    /// Watches the microphone: device changes, repairs, notifications.
    fn poll_health(&mut self) {
        let now = Instant::now();
        let target = health_target(&self.state, &self.selected_mic);
        if target != self.health_target {
            self.health_watch.set_target(&target);
            self.health_target = target;
        }
        if let Some(update) = self.health_watch.poll() {
            self.health_probe = Some(update.probe);
            if update.devices_changed {
                // Keeps the device picker from going stale.
                self.refresh_mics();
            }
        }
        let finished = match &self.repair_job {
            Some((kind, job)) => match job.try_recv() {
                Ok(outcome) => Some((*kind, Some(outcome))),
                Err(mpsc::TryRecvError::Disconnected) => Some((*kind, None)),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        if let Some((kind, outcome)) = finished {
            self.repair_job = None;
            let success = outcome.as_ref().is_some_and(|o| o.success);
            self.health_monitor.action_finished(kind, success, now);
            if !success {
                self.repair_note = Some(
                    outcome
                        .map(|o| o.message)
                        .unwrap_or_else(|| "The repair did not finish.".into()),
                );
            }
            self.state = install_state::read();
            self.state_checked = Instant::now();
        }
        let Some(probe) = self.health_probe.clone() else {
            return;
        };
        let snapshot = health::Snapshot {
            mic: self.health_target.clone(),
            installed: !self.state.attached.is_empty(),
            clean_mic_on: self.settings.enabled,
            probe,
            live_age_ms: self.live_levels.map(|levels| levels.age_ms()),
        };
        let config = health::Config {
            auto_repair: self.prefs.auto_repair,
            hold_pressed: self.comparing,
        };
        let step = self.health_monitor.step(&snapshot, config, now);
        self.health = step.health;
        if let Some(kind) = step.action {
            // Automatic repairs never pop UAC; only "Fix now" may.
            self.start_repair(kind, false, false);
        }
        if let (Some(notice), true) = (step.notice, self.prefs.notifications) {
            let (title, text) = notice.text();
            notify::show(&title, &text);
        }
    }

    fn health_banner(&mut self, ui: &mut egui::Ui) {
        let fixing = self.repair_job.is_some();
        if self.health == health::Health::Healthy && !fixing {
            return;
        }
        let p = palette(ui);
        let informational = !self.health.needs_attention();
        let message = if fixing {
            "Fixing Clean Mic…".to_string()
        } else {
            self.health.message(&self.health_target)
        };
        let fix = self.health.fix().filter(|_| !fixing);
        let mut clicked = false;
        theme::card(ui, None, None, |ui| {
            ui.horizontal(|ui| {
                let text_width = (ui.available_width() - 110.0).max(150.0);
                ui.vertical(|ui| {
                    ui.set_width(text_width);
                    let color = if informational { p.muted_foreground } else { p.warning };
                    ui.label(RichText::new(message).small().color(color));
                    if let Some(note) = &self.repair_note {
                        theme::hint(ui, note);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if fixing {
                        ui.spinner();
                    } else if fix.is_some() && theme::outline_button(ui, "Fix now").clicked() {
                        clicked = true;
                    }
                });
            });
        });
        ui.add_space(12.0);
        if let (true, Some(kind)) = (clicked, fix) {
            // The user asked for it, so the UAC fallback is allowed.
            self.start_repair(kind, true, true);
        }
    }

    fn open_recordings_folder(&self) {
        let dir = mic_test::recordings_dir();
        // Explorer falls back to Documents if the folder does not exist yet.
        let _ = std::fs::create_dir_all(&dir);
        open_in_shell(&dir.display().to_string());
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

    /// "Hold to hear original" button. Returns while pressed.
    fn compare_button(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let label = if self.comparing {
            theme::semibold_text("Hearing original…").color(p.primary_foreground)
        } else {
            RichText::new("Hold to hear original")
        };
        let response = ui.add(
            egui::Button::new(label)
                .fill(if self.comparing {
                    p.primary
                } else {
                    p.background
                })
                .stroke(egui::Stroke::new(
                    1.0,
                    if self.comparing { p.primary } else { p.border },
                ))
                .corner_radius(6)
                .sense(egui::Sense::click_and_drag()),
        );
        let held = response.is_pointer_button_down_on();
        if held != self.comparing {
            self.comparing = held;
            self.save_now();
        }
    }

    /// Live meters from the plugin (works whenever any app uses the mic).
    fn live_card(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let live = self.live_levels.filter(|l| l.age_ms() < 1500);
        theme::card(
            ui,
            Some("Live"),
            Some("What another app (Discord, a game) is receiving right now. Hush itself never records you."),
            |ui| {
            match live {
                None => theme::hint(
                    ui,
                    "No app is using your microphone right now. Join a Discord call, or use Mic Test, to see the meters.",
                ),
                Some(levels) => {
                    let meter = |ui: &mut egui::Ui, label: &str, value: f32| {
                        ui.horizontal(|ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(120.0, 16.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.set_min_width(120.0);
                                    ui.label(RichText::new(label).small());
                                },
                            );
                            let db = 20.0 * value.max(1e-5).log10();
                            let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width() - 4.0, 8.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(rect, 4.0, p.muted);
                            let mut bar = rect;
                            bar.set_width(rect.width() * fill);
                            ui.painter().rect_filled(bar, 4.0, p.primary);
                        });
                    };
                    meter(ui, "Your mic (raw)", levels.input);
                    meter(ui, "What others hear", levels.output);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let (color, text) = if levels.voice {
                            (p.success, "Speaking: passing your voice")
                        } else {
                            (egui::Color32::GRAY, "Quiet: others hear silence")
                        };
                        theme::badge(ui, color, text);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            self.compare_button(ui);
                        });
                    });
                }
            }
        });
        ui.add_space(12.0);
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
        self.test = None;
        self.monitor = false;
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
    }

    fn start_test(&mut self) {
        match mic_test::MicTest::start(&self.selected_mic) {
            Ok(test) => {
                test.set_monitor(self.monitor);
                self.test = Some(test);
                self.test_error = None;
            }
            Err(error) => self.test_error = Some(format!("{error:#}")),
        }
    }

    fn run_setup(&mut self, args: Vec<String>) {
        self.test = None;
        self.monitor = false;
        self.setup_job = Some(install_state::run_setup(args));
        self.setup_message = Some("Waiting for Windows permission…".into());
    }

    fn poll_background(&mut self) {
        if let Some(job) = &self.setup_job {
            if let Ok(message) = job.try_recv() {
                self.setup_message = Some(message);
                self.setup_job = None;
                self.state = install_state::read();
                self.state_checked = Instant::now();
            }
        }
        if let Some(job) = &self.file_job {
            match job.try_recv() {
                Ok(settings_io::DialogResult::Exported(path)) => {
                    self.tuning_message =
                        Some((format!("Exported to {}.", path.display()), true));
                    self.file_job = None;
                }
                Ok(settings_io::DialogResult::Imported(imported)) => {
                    self.apply_loaded_settings(imported);
                    self.tuning_message = Some((
                        "Imported. Clean Mic stays as it was (on or off).".into(),
                        true,
                    ));
                    self.file_job = None;
                }
                Ok(settings_io::DialogResult::Failed(message)) => {
                    self.tuning_message = Some((message, false));
                    self.file_job = None;
                }
                // The dialog was cancelled.
                Err(mpsc::TryRecvError::Disconnected) => self.file_job = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(job) = &self.update_job {
            match job.try_recv() {
                Ok(result) => {
                    self.update_result = Some(result);
                    self.update_job = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.update_result = Some(Err("The update check did not finish.".into()));
                    self.update_job = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.state_checked.elapsed() >= Duration::from_secs(1) {
            self.state = install_state::read();
            self.state_checked = Instant::now();
        }
        if let Some(test) = &self.test {
            let peak = test.peak();
            self.meter = if peak > self.meter {
                peak
            } else {
                self.meter * 0.85 + peak * 0.15
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

    // -- Layout --------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let logo = self.logo.get_or_insert_with(|| {
                ui.ctx().load_texture(
                    "hush-logo",
                    egui::ColorImage::from_rgba_unmultiplied(
                        [64, 64],
                        &crate::icon_art::render_icon(64),
                    ),
                    egui::TextureOptions::LINEAR,
                )
            });
            ui.add(egui::Image::new((logo.id(), egui::vec2(28.0, 28.0))));
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(theme::semibold_text("Hush").size(17.0));
                ui.label(
                    RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                        .size(11.0)
                        .color(p.muted_foreground),
                );
            });
        });
        ui.add_space(18.0);
        let mut nav = |ui: &mut egui::Ui, page: Page| {
            let selected = self.page == page;
            let text = RichText::new(page.label()).color(if selected {
                p.foreground
            } else {
                p.muted_foreground
            });
            let button = egui::Button::new(text)
                .fill(if selected {
                    p.muted
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(egui::Stroke::NONE)
                .corner_radius(6)
                .min_size(egui::vec2(ui.available_width(), 34.0));
            let response = ui
                .add(button)
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if selected {
                let r = response.rect;
                let bar = egui::Rect::from_min_size(
                    egui::pos2(r.left(), r.center().y - 8.0),
                    egui::vec2(3.0, 16.0),
                );
                ui.painter().rect_filled(bar, 2.0, p.primary);
            }
            if response.clicked() {
                self.page = page;
            }
        };
        for page in Page::MAIN {
            nav(ui, page);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(12.0);
            for page in Page::FOOTER.iter().rev() {
                nav(ui, *page);
            }
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let (color, text) = self.status();
            ui.label(theme::semibold_text("Clean Mic").size(16.0));
            theme::badge(ui, color, text);
            let talking = self
                .live_levels
                .is_some_and(|l| l.voice && l.age_ms() < 1500 && self.settings.enabled);
            if talking {
                theme::badge(ui, palette(ui).primary, "Talking");
            }
            if self.mute.is_muted() {
                theme::badge(ui, palette(ui).destructive, "Muted");
            }
            match self.health {
                health::Health::Disconnected => {
                    theme::badge(ui, palette(ui).warning, "Disconnected");
                }
                health::Health::Missing
                | health::Health::NotProcessing
                | health::Health::Bypassed
                | health::Health::Failing => {
                    theme::badge(ui, palette(ui).warning, "Not processing");
                }
                _ => {}
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::switch(ui, &mut self.settings.enabled, "Clean Mic")
                    .on_hover_text("Turn all microphone processing on or off")
                    .changed()
                {
                    self.mark_dirty();
                }
            });
        });
        ui.add_space(6.0);
    }

    // -- Pages ---------------------------------------------------------------

    fn microphone_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Microphone",
            "Choose your microphone and install Clean Mic on it. Apps keep using that microphone as usual.",
        );
        self.health_banner(ui);
        let attached_here = self.attached_here();
        let mut refresh = false;
        theme::card(ui, Some("Device"), None, |ui| {
            theme::row_sized(
                ui,
                "Microphone",
                "The physical microphone to clean up.",
                theme::COMBO_WIDTH + 84.0,
                |ui| {
                    if theme::outline_button(ui, "Refresh")
                        .on_hover_text("Rescan microphones")
                        .clicked()
                    {
                        refresh = true;
                    }
                    let mut chosen = self.selected_mic.clone();
                    egui::ComboBox::from_id_salt("mic")
                        .selected_text(chosen.clone())
                        .show_ui(ui, |ui| {
                            for mic in &self.mics {
                                ui.selectable_value(&mut chosen, mic.clone(), mic);
                            }
                        });
                    if chosen != self.selected_mic {
                        self.selected_mic = chosen;
                        self.mute.set_device(&self.selected_mic);
                        if self.test.take().is_some() {
                            self.start_test();
                        }
                    }
                },
            );
            theme::separator(ui);
            theme::row_sized(
                ui,
                "Mic position",
                "On the desk gives a distant mic more boost and protects softer word endings.",
                theme::MIC_POSITION_WIDTH,
                |ui| {
                    let mut position = Some(self.settings.desk_mic);
                    theme::mic_position(ui, &mut position, false);
                    if let Some(desk_mic) =
                        position.filter(|&desk_mic| desk_mic != self.settings.desk_mic)
                    {
                        self.settings.desk_mic = desk_mic;
                        self.mark_dirty();
                    }
                },
            );
            theme::separator(ui);
            let busy = self.setup_job.is_some();
            theme::row(
                ui,
                "Clean Mic",
                if attached_here {
                    "Installed on this microphone."
                } else {
                    "Not installed on this microphone yet."
                },
                |ui| {
                    if busy {
                        ui.spinner();
                    } else if attached_here {
                        if theme::destructive_button(ui, "Remove").clicked() {
                            self.run_setup(vec!["uninstall".into()]);
                        }
                    } else if theme::primary_button(ui, "Install").clicked() {
                        let args =
                            vec!["install".into(), "--mic".into(), self.selected_mic.clone()];
                        self.run_setup(args);
                    }
                },
            );
            if let Some(message) = &self.setup_message {
                theme::hint(ui, message);
            }
        });
        if refresh {
            self.refresh_mics();
        }
        if let Some(warning) = self.level_warning {
            ui.add_space(12.0);
            let p = palette(ui);
            let message = match warning {
                level_watch::LevelWarning::Clipping => "Your microphone is clipping. Lower its input level in Windows Sound settings (or the mic's own gain) until loud words stay below the top.",
                level_watch::LevelWarning::TooQuiet => "Your microphone is very quiet. Raise its input level in Windows Sound settings so Hush can tell your voice from noise.",
            };
            theme::card(ui, None, None, |ui| {
                ui.horizontal(|ui| {
                    let text_width = (ui.available_width() - 130.0).max(150.0);
                    ui.vertical(|ui| {
                        ui.set_width(text_width);
                        ui.label(RichText::new(message).small().color(p.warning));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::outline_button(ui, "Sound settings").clicked() {
                            open_in_shell("ms-settings:sound");
                        }
                    });
                });
            });
        }
        ui.add_space(12.0);
        theme::card(ui, Some("How it works"), None, |ui| {
            theme::hint(
                ui,
                "Clean Mic runs inside Windows' own audio engine, so it works in every app, even when this window is closed.",
            );
            theme::hint(
                ui,
                &format!(
                    "In Discord and games, select \"{}\" directly (not the SteelSeries Sonar microphone), or your voice will be processed twice.",
                    if self.selected_mic.is_empty() { "your microphone" } else { &self.selected_mic }
                ),
            );
            theme::hint(
                ui,
                "Installing asks for administrator permission once and restarts Windows audio for a moment.",
            );
        });
        if attached_here && self.state.glitches > 0 {
            ui.add_space(12.0);
            let p = palette(ui);
            theme::card(ui, Some("Audio gaps detected"), None, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} short gaps since Windows audio started. If you hear crackles, close heavy background apps and tell us.",
                        self.state.glitches
                    ))
                    .small()
                    .color(p.warning),
                );
            });
        }
    }

    fn noise_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Noise & Gate",
            "Remove background noise and silence the mic between words.",
        );
        self.live_card(ui);
        let mut changed = false;
        ui.add_enabled_ui(self.settings.enabled, |ui| {
            theme::card(ui, Some("Noise cancellation"), Some("AI noise removal (DeepFilterNet 3): keyboard, fans, clicks, room noise."), |ui| {
                theme::row(ui, "Enabled", "", |ui| {
                    changed |= theme::switch(ui, &mut self.settings.noise_suppression, "Noise cancellation").changed();
                });
                theme::separator(ui);
                ui.add_enabled_ui(self.settings.noise_suppression, |ui| {
                    theme::row_sized(ui, "Strength", "Lower sounds more natural; higher removes more.", theme::SLIDER_WIDTH, |ui| {
                        let mut strength = self.settings.suppression * 100.0;
                        if ui
                            .add(egui::Slider::new(&mut strength, 0.0..=100.0).suffix(" %").integer())
                            .changed()
                        {
                            self.settings.suppression = strength / 100.0;
                            changed = true;
                        }
                    });
                    theme::separator(ui);
                    theme::row(
                        ui,
                        "Click suppression",
                        "Softens hard keyboard and mouse clicks while you talk.",
                        |ui| {
                            changed |= theme::switch(ui, &mut self.settings.click_suppression, "Click suppression").changed();
                        },
                    );
                });
            });
            ui.add_space(12.0);
            ui.add_enabled_ui(self.settings.noise_suppression, |ui| {
                theme::card(ui, Some("Voice gate"), Some("Closes the mic when you are not talking. Short sounds like key clicks never open it."), |ui| {
                    theme::row(ui, "Enabled", "", |ui| {
                        changed |= theme::switch(ui, &mut self.settings.gate, "Voice gate").changed();
                    });
                    theme::separator(ui);
                    ui.add_enabled_ui(self.settings.gate, |ui| {
                        theme::row_sized(
                            ui,
                            "Sensitivity",
                            "Lower if noises open the mic; raise if soft words get cut.",
                            theme::SLIDER_WIDTH,
                            |ui| {
                                let mut sensitivity = self.settings.gate_sensitivity * 100.0;
                                if ui
                                    .add(egui::Slider::new(&mut sensitivity, 0.0..=100.0).suffix(" %").integer())
                                    .changed()
                                {
                                    self.settings.gate_sensitivity = sensitivity / 100.0;
                                    changed = true;
                                }
                            },
                        );
                    });
                });
            });
        });
        if changed {
            self.mark_dirty();
        }
    }

    fn voice_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Voice",
            "Shape the tone of your voice and keep its volume even.",
        );
        let mut changed = false;
        ui.add_enabled_ui(self.settings.enabled, |ui| {
            theme::card(
                ui,
                Some("Preset"),
                Some("A starting point; adjust the sliders to taste."),
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let p = palette(ui);
                        for preset in Preset::ALL {
                            let selected = self.settings.preset == preset;
                            let button = egui::Button::new(if selected {
                                theme::semibold_text(preset.label()).color(p.primary_foreground)
                            } else {
                                RichText::new(preset.label())
                            })
                            .fill(if selected { p.primary } else { p.background })
                            .stroke(egui::Stroke::new(
                                1.0,
                                if selected { p.primary } else { p.border },
                            ))
                            .corner_radius(6);
                            if ui.add(button).clicked() && !selected {
                                preset.apply(&mut self.settings);
                                changed = true;
                            }
                        }
                    });
                },
            );
            ui.add_space(12.0);
            theme::card(ui, Some("Equalizer"), None, |ui| {
                let mut eq_changed = false;
                theme::row_sized(
                    ui,
                    "Bass",
                    "Body and fullness.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        eq_changed |= ui
                            .add(
                                egui::Slider::new(&mut self.settings.bass_db, -6.0..=9.0)
                                    .suffix(" dB")
                                    .step_by(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Clarity",
                    "Presence; helps you cut through.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        eq_changed |= ui
                            .add(
                                egui::Slider::new(&mut self.settings.clarity_db, -6.0..=9.0)
                                    .suffix(" dB")
                                    .step_by(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Air",
                    "Crispness. Too much can sound hissy.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        eq_changed |= ui
                            .add(
                                egui::Slider::new(&mut self.settings.air_db, -6.0..=9.0)
                                    .suffix(" dB")
                                    .step_by(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Saturation",
                    "Subtle tape-style warmth, like a good preamp. 0 is off.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        let mut warmth = self.settings.warmth * 100.0;
                        if ui
                            .add(
                                egui::Slider::new(&mut warmth, 0.0..=100.0)
                                    .suffix(" %")
                                    .integer(),
                            )
                            .changed()
                        {
                            self.settings.warmth = warmth / 100.0;
                            changed = true;
                        }
                    },
                );
                if eq_changed {
                    self.settings.preset = Preset::Custom;
                    changed = true;
                }
            });
            ui.add_space(12.0);
            theme::card(ui, Some("Level"), None, |ui| {
                theme::row(
                    ui,
                    "Auto level",
                    "Keeps your voice at a steady volume, like a broadcast engineer would.",
                    |ui| {
                        changed |=
                            theme::switch(ui, &mut self.settings.leveler, "Auto level").changed();
                    },
                );
                theme::separator(ui);
                theme::row(
                    ui,
                    "De-esser",
                    "Softens harsh \"s\" and \"t\" sounds.",
                    |ui| {
                        changed |=
                            theme::switch(ui, &mut self.settings.deesser, "De-esser").changed();
                    },
                );
                theme::separator(ui);
                theme::row(
                    ui,
                    "Rumble & pop filter",
                    "Removes desk bumps, low hum and \"p\"/\"b\" pops.",
                    |ui| {
                        changed |=
                            theme::switch(ui, &mut self.settings.high_pass, "Rumble & pop filter")
                                .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(ui, "Output volume", "", theme::SLIDER_WIDTH, |ui| {
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut self.settings.gain_db, -12.0..=12.0)
                                .suffix(" dB")
                                .step_by(0.5)
                                .fixed_decimals(1),
                        )
                        .changed();
                });
            });
        });
        if changed {
            self.mark_dirty();
        }
    }

    fn test_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Mic Test",
            "Hear exactly what others hear. Toggle settings while listening to compare.",
        );
        let p = palette(ui);
        if self.test.is_none() {
            theme::card(ui, None, None, |ui| {
                theme::row(
                    ui,
                    "Microphone test",
                    "Opens the microphone the same way Discord does.",
                    |ui| {
                        if theme::primary_button(ui, "Start test").clicked() {
                            self.start_test();
                        }
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Saved recordings",
                    "Takes you saved with Save are kept in this folder.",
                    170.0,
                    |ui| {
                        if theme::outline_button(ui, "Open recordings folder").clicked() {
                            self.open_recordings_folder();
                        }
                    },
                );
                if let Some(error) = &self.test_error {
                    ui.label(RichText::new(error).small().color(p.destructive));
                }
            });
            return;
        }

        let level_db = 20.0 * self.meter.max(1e-5).log10();
        let fill = ((level_db + 60.0) / 60.0).clamp(0.0, 1.0);
        theme::card(ui, Some("Input level"), None, |ui| {
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 5.0, p.muted);
            let mut bar = rect;
            bar.set_width(rect.width() * fill);
            let color = if level_db > -3.0 {
                p.destructive
            } else if level_db > -12.0 {
                p.warning
            } else {
                p.primary
            };
            ui.painter().rect_filled(bar, 5.0, color);
            theme::hint(ui, &format!("{level_db:.0} dB"));
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Compare"), None, |ui| {
            theme::row(
                ui,
                "Original vs. Clean Mic",
                "With Hear myself on, hold the button to hear your raw microphone.",
                |ui| self.compare_button(ui),
            );
        });
        ui.add_space(12.0);
        let mut stop = false;
        let mut saved = None;
        if let Some(test) = &self.test {
            theme::card(ui, Some("Listen"), None, |ui| {
                theme::row(
                    ui,
                    "Hear myself",
                    &format!("Plays through {}. Use headphones.", test.output_name),
                    |ui| {
                        if theme::switch(ui, &mut self.monitor, "Hear myself").changed() {
                            test.set_monitor(self.monitor);
                        }
                    },
                );
                theme::separator(ui);
                theme::row(
                    ui,
                    "Record and play back",
                    &format!(
                        "Records {} seconds. Try once with Clean Mic on and once off.",
                        mic_test::RECORD_SECONDS
                    ),
                    |ui| match test.recording_progress() {
                        Some(progress) => {
                            if theme::outline_button(ui, "Stop").clicked() {
                                test.stop_recording();
                            }
                            ui.add(egui::ProgressBar::new(progress).desired_width(120.0));
                        }
                        None => {
                            if test.is_playing() {
                                if theme::outline_button(ui, "Stop playback").clicked() {
                                    test.stop_playback();
                                }
                            } else if ui
                                .add_enabled(
                                    test.has_recording(),
                                    egui::Button::new("Play").corner_radius(6),
                                )
                                .clicked()
                            {
                                test.play_recording();
                            }
                            if test.has_recording() && theme::outline_button(ui, "Save").clicked() {
                                saved = Some(test.save_recording().map_err(|e| format!("{e:#}")));
                            }
                            if theme::outline_button(ui, "Record").clicked() {
                                test.start_recording();
                            }
                        }
                    },
                );
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if theme::outline_button(ui, "Stop test").clicked() {
                    stop = true;
                }
                if theme::outline_button(ui, "Open recordings folder").clicked() {
                    self.open_recordings_folder();
                }
            });
        }
        if stop {
            self.test = None;
            self.monitor = false;
        }
        if let Some(result) = saved {
            self.saved_recording = Some((result, Instant::now()));
        }
    }

    /// Toast in the bottom-right corner after "Save" in Mic Test: the file
    /// path with Copy / Show in folder. Closes itself after 10 s unless the
    /// pointer is over it.
    fn recording_toast(&mut self, ctx: &egui::Context) {
        let Some((result, shown)) = &self.saved_recording else {
            return;
        };
        let p = if ctx.global_style().visuals.dark_mode {
            theme::DARK
        } else {
            theme::LIGHT
        };
        let mut close = false;
        let response = egui::Area::new(egui::Id::unique("recording-toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(p.card)
                    .stroke(egui::Stroke::new(1.0, p.border))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(14))
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 16,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(80),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(380.0);
                        ui.horizontal(|ui| {
                            let title = if result.is_ok() {
                                "Recording saved"
                            } else {
                                "Could not save recording"
                            };
                            ui.label(theme::semibold_text(title));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(egui::Button::new("×").frame(false))
                                        .on_hover_text("Close")
                                        .clicked()
                                    {
                                        close = true;
                                    }
                                },
                            );
                        });
                        match result {
                            Ok(path) => {
                                let text = path.display().to_string();
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&text).small().color(p.muted_foreground),
                                    )
                                    .wrap(),
                                );
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    if theme::primary_button(ui, "Copy path").clicked() {
                                        close = crate::clipboard::copy(&text);
                                    }
                                    if theme::outline_button(ui, "Show in folder").clicked() {
                                        let _ = std::process::Command::new("explorer.exe")
                                            .arg(format!("/select,{text}"))
                                            .spawn();
                                    }
                                });
                            }
                            Err(error) => {
                                ui.label(RichText::new(error).small().color(p.destructive));
                            }
                        }
                    });
            })
            .response;
        let hovered = response.contains_pointer();
        if close || (!hovered && shown.elapsed() > Duration::from_secs(10)) {
            self.saved_recording = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn settings_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(ui, "Settings", "How Hush starts and behaves.");
        let p = palette(ui);
        theme::card(ui, Some("Startup"), None, |ui| {
            theme::row(
                ui,
                "Start with Windows",
                "Starts quietly in the system tray when you sign in.",
                |ui| {
                    if theme::switch(ui, &mut self.start_with_windows, "Start with Windows")
                        .changed()
                    {
                        self.prefs_error = prefs::set_start_with_windows(self.start_with_windows)
                            .err()
                            .map(|e| format!("Could not change startup setting: {e}"));
                        self.start_with_windows = prefs::start_with_windows();
                    }
                },
            );
            theme::separator(ui);
            theme::row(
                ui,
                "Keep running in the tray",
                "Closing the window hides it to the tray instead of quitting.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.close_to_tray, "Close to tray").changed() {
                        self.prefs_error = prefs::save(&self.prefs)
                            .err()
                            .map(|e| format!("Could not save: {e}"));
                    }
                },
            );
            if let Some(error) = &self.prefs_error {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Microphone health"), None, |ui| {
            theme::row(
                ui,
                "Repair Clean Mic automatically",
                "If your mic reconnects without Clean Mic, Hush puts it back. This can briefly restart Windows audio.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.auto_repair, "Repair automatically")
                        .changed()
                    {
                        self.save_prefs();
                    }
                },
            );
            theme::separator(ui);
            theme::row(
                ui,
                "Notifications",
                "Tells you when your microphone disconnects or Clean Mic stops working.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.notifications, "Notifications").changed() {
                        self.save_prefs();
                    }
                },
            );
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Mute"), None, |ui| {
            theme::row(
                ui,
                "Mute hotkey",
                "Mutes your microphone in every app. Hush listens only for this key.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.mute_hotkey, "Mute hotkey").changed() {
                        self.save_prefs();
                        self.apply_hotkey();
                    }
                },
            );
            if self.prefs.mute_hotkey {
                theme::separator(ui);
                theme::row_sized(ui, "Key", "", 160.0, |ui| {
                    let mut key = self.prefs.mute_key;
                    egui::ComboBox::from_id_salt("mute-key")
                        .selected_text(key.label())
                        .show_ui(ui, |ui| {
                            for option in prefs::MuteKey::ALL {
                                ui.selectable_value(&mut key, option, option.label());
                            }
                        });
                    if key != self.prefs.mute_key {
                        self.prefs.mute_key = key;
                        self.save_prefs();
                        self.apply_hotkey();
                    }
                });
            }
            if let Some(error) = &self.hotkey_error {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
            theme::separator(ui);
            let muted = self.mute.is_muted();
            theme::row(
                ui,
                "Microphone",
                if self.selected_mic.is_empty() {
                    "Uses the Windows mute switch."
                } else {
                    "Uses the Windows mute switch of the selected microphone."
                },
                |ui| {
                    if muted {
                        if theme::outline_button(ui, "Unmute").clicked() {
                            self.mute.toggle();
                        }
                        theme::badge(ui, p.destructive, "Muted");
                    } else if theme::outline_button(ui, "Mute").clicked() {
                        self.mute.toggle();
                    }
                },
            );
            if let Some(error) = self.mute.error() {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Microphone settings"), None, |ui| {
            theme::row_sized(
                ui,
                "Reset to defaults",
                "Restores noise, gate and voice settings.",
                190.0,
                |ui| {
                    let armed = self
                        .reset_armed
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(4));
                    if armed {
                        ui.ctx().request_repaint_after(Duration::from_millis(250));
                        if theme::destructive_button(ui, "Click again to reset").clicked() {
                            self.settings = MicSettings::default();
                            self.mark_dirty();
                            self.reset_armed = None;
                        }
                    } else if theme::outline_button(ui, "Reset").clicked() {
                        self.reset_armed = Some(Instant::now());
                    }
                },
            );
            theme::separator(ui);
            let snapshot_note = match self.snapshot_saved {
                Some(saved) => format!(
                    "Saved {}. Restoring keeps Clean Mic on or off as it is.",
                    settings_io::ago(
                        SystemTime::now()
                            .duration_since(saved)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    )
                ),
                None => "Keep a copy of your noise, gate and voice settings to come back to."
                    .to_string(),
            };
            theme::row_sized(ui, "Tuning snapshot", &snapshot_note, 250.0, |ui| {
                if ui
                    .add_enabled_ui(self.snapshot_saved.is_some(), |ui| {
                        theme::outline_button(ui, "Restore tuning")
                    })
                    .inner
                    .clicked()
                {
                    self.tuning_message = Some(match settings_io::load_snapshot() {
                        Ok(snapshot) => {
                            self.apply_loaded_settings(snapshot);
                            ("Restored your saved tuning.".to_string(), true)
                        }
                        Err(message) => (message, false),
                    });
                }
                if theme::outline_button(ui, "Save tuning").clicked() {
                    self.tuning_message = Some(match settings_io::save_snapshot(&self.settings) {
                        Ok(()) => {
                            self.snapshot_saved = settings_io::snapshot_time();
                            ("Saved your tuning.".to_string(), true)
                        }
                        Err(e) => (format!("Could not save tuning: {e}"), false),
                    });
                }
            });
            theme::separator(ui);
            theme::row_sized(
                ui,
                "Export or import",
                "Share your settings as a .conf file.",
                190.0,
                |ui| {
                    ui.add_enabled_ui(self.file_job.is_none(), |ui| {
                        if theme::outline_button(ui, "Import\u{2026}").clicked() {
                            self.file_job = Some(settings_io::import(self.wake()));
                        }
                        if theme::outline_button(ui, "Export\u{2026}").clicked() {
                            self.file_job = Some(settings_io::export(self.settings, self.wake()));
                        }
                    });
                },
            );
            if let Some((message, ok)) = &self.tuning_message {
                let color = if *ok { p.success } else { p.destructive };
                ui.label(RichText::new(message).small().color(color));
            }
            theme::separator(ui);
            theme::row(
                ui,
                "Settings folder",
                "Where mic settings and diagnostics are stored.",
                |ui| {
                    if theme::outline_button(ui, "Open").clicked() {
                        let _ = std::process::Command::new("explorer.exe")
                            .arg(settings::settings_dir())
                            .spawn();
                    }
                },
            );
            if let Some(error) = &self.save_error {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Support"), None, |ui| {
            theme::row_sized(
                ui,
                "Diagnostics",
                "Copies a text summary to paste into a bug report. No audio is included.",
                160.0,
                |ui| self.diagnostics_button(ui),
            );
        });
    }

    fn about_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(ui, "About", &format!("Hush {}", env!("CARGO_PKG_VERSION")));
        theme::card(ui, Some("What's inside"), None, |ui| {
            theme::hint(
                ui,
                "Noise removal: DeepFilterNet 3 (MIT / Apache-2.0), running on your CPU.",
            );
            theme::hint(ui, "Audio plugin: runs inside Windows' audio engine (audiodg.exe) on the selected microphone.");
            theme::hint(
                ui,
                "No virtual devices and no kernel drivers are installed.",
            );
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Status"), None, |ui| {
            let attached = if self.state.attached.is_empty() {
                "none".to_string()
            } else {
                self.state.attached.join(", ")
            };
            theme::hint(ui, &format!("Installed on: {attached}"));
            theme::hint(
                ui,
                &format!(
                    "Audio engine: {}",
                    if self.state.running {
                        "processing"
                    } else {
                        "idle"
                    }
                ),
            );
            theme::hint(
                ui,
                &format!(
                    "Audio gaps since Windows audio started: {}",
                    self.state.glitches
                ),
            );
            ui.add_space(4.0);
            self.diagnostics_button(ui);
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Help"), None, |ui| {
            let program_dir = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()));
            let notices = program_dir
                .as_ref()
                .map(|dir| dir.join("THIRD-PARTY-NOTICES.txt"))
                .filter(|path| path.exists());
            let repository = env!("CARGO_PKG_REPOSITORY");
            ui.horizontal(|ui| {
                if !repository.is_empty() && theme::outline_button(ui, "Report a problem").clicked()
                {
                    open_in_shell(&format!("{repository}/issues"));
                }
                if let Some(notices) = &notices {
                    if theme::outline_button(ui, "Licenses").clicked() {
                        open_in_shell(&notices.display().to_string());
                    }
                }
                // Only offered when the build knows its GitHub repository.
                if updates::parse_repository(repository).is_some()
                    && self.update_job.is_none()
                    && theme::outline_button(ui, "Check for updates").clicked()
                {
                    self.update_result = None;
                    self.update_job = updates::check(repository);
                }
            });
            if updates::parse_repository(repository).is_some() {
                if self.update_job.is_some() {
                    theme::hint(ui, "Checking\u{2026}");
                }
                match &self.update_result {
                    Some(Ok(updates::Outcome::UpToDate)) => {
                        ui.label(
                            RichText::new("You have the latest version.")
                                .small()
                                .color(palette(ui).success),
                        );
                    }
                    Some(Ok(updates::Outcome::Available { version, url })) => {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("Hush {version} is available."))
                                    .small()
                                    .color(palette(ui).success),
                            );
                            if theme::outline_button(ui, "Open release page").clicked() {
                                open_in_shell(url);
                            }
                        });
                    }
                    Some(Err(error)) => {
                        ui.label(RichText::new(error).small().color(palette(ui).destructive));
                    }
                    None => {}
                }
                theme::hint(
                    ui,
                    "Checks GitHub once when you click. Hush never checks on its own.",
                );
            }
            theme::hint(
                ui,
                "To uninstall Hush completely, use Windows Settings > Apps > Installed apps.",
            );
        });
    }
}

impl HushApp {
    fn developer_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, started, requested)) = &mut self.shot else {
            return;
        };
        let delay = std::env::var("HUSH_SHOT_DELAY")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .unwrap_or(1500);
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
            tray.sync(self.settings.enabled, muted, self.health.needs_attention());
            if self.health.needs_attention() && !muted {
                tray.set_tooltip(&format!("Hush – {}", self.health.short()));
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
            || self.setup_job.is_some()
            || self.update_job.is_some()
            || self.diag_copied.is_some();
        let live = self.live_levels.is_some_and(|l| l.age_ms() < 1500);
        if self.visible && (self.test.is_some() || busy || live || self.comparing) {
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
fn open_in_shell(target: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn();
}
