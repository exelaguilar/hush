//! Hush installer: a small wizard that installs the app to
//! Program Files, attaches Clean Mic to the chosen microphone, adds a Start
//! menu shortcut and an "Installed apps" entry, and optionally starts
//! Hush with Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
#[path = "../../src/icon_art.rs"]
mod icon_art;
#[cfg(windows)]
#[path = "../../src/theme.rs"]
#[allow(dead_code)]
mod theme;

#[cfg(windows)]
fn main() -> eframe::Result {
    let icon = eframe::egui::IconData {
        rgba: icon_art::render_icon(64),
        width: 64,
        height: 64,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Hush Setup")
            .with_inner_size([600.0, 510.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_icon(icon),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Hush Setup",
        options,
        Box::new(|cc| Ok(Box::new(wizard::Wizard::new(&cc.egui_ctx)))),
    )
}

#[cfg(windows)]
#[path = "../../src/windows_setup_paths.rs"]
mod windows_setup_paths;

#[cfg(windows)]
mod install {
    //! The actual installation steps (run on a worker thread).
    use crate::windows_setup_paths::{program_files, replace_file, system_tool};
    use std::{
        fs,
        io::Write,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::Command,
        sync::{Arc, Mutex},
    };
    use winreg::{
        enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ},
        RegKey,
    };

    const NO_WINDOW: u32 = 0x0800_0000;
    pub const APP_EXE: &[u8] = include_bytes!(concat!(env!("SM_PAYLOAD_DIR"), "/hush.exe"));
    pub const SETUP_EXE: &[u8] = include_bytes!(concat!(env!("SM_PAYLOAD_DIR"), "/hush-setup.exe"));
    pub const APO_DLL: &[u8] = include_bytes!(concat!(env!("SM_PAYLOAD_DIR"), "/hush_apo.dll"));
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");
    /// Project page shown in Settings > Apps (empty until the repository
    /// URL in Cargo.toml is set).
    const HOMEPAGE: &str = env!("CARGO_PKG_REPOSITORY");
    const LICENSE: &str = include_str!("../../LICENSE");
    const NOTICES: &str = include_str!("../../THIRD-PARTY-NOTICES.txt");
    const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Hush";

    pub struct Options {
        pub microphone: Option<String>,
        pub desk_mic: Option<bool>,
        pub start_with_windows: bool,
    }

    pub struct Microphone {
        pub id: String,
        pub name: String,
    }

    pub type Log = Arc<Mutex<Vec<String>>>;
    /// `Ok(Some(warning))`: installed with a microphone setup warning.
    pub type Outcome = Result<Option<String>, String>;

    fn log(log: &Log, line: impl Into<String>) {
        log.lock().unwrap().push(line.into());
    }

    fn program_dir() -> std::io::Result<PathBuf> {
        Ok(program_files()?.join("Hush"))
    }

    fn data_dir() -> PathBuf {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
    }

    pub fn has_saved_mic_settings() -> bool {
        let data = data_dir();
        data.join(r"Hush\mic.conf").is_file()
            || (program_files()
                .is_ok_and(|root| root.join(r"SteelMixer\steelmixer-setup.exe").is_file())
                && data.join(r"SteelMixer\mic.conf").is_file())
    }

    fn save_mic_position(desk_mic: bool) -> std::io::Result<()> {
        // The setup helper has already created the file and the app is closed.
        // Preserve the rest of the file, including keys from other versions.
        let path = data_dir().join(r"Hush\mic.conf");
        let text = fs::read_to_string(&path)?;
        let mut updated: String = text
            .split_inclusive('\n')
            .filter(|line| {
                !line
                    .split_once('=')
                    .is_some_and(|(key, _)| key.trim() == "desk_mic")
            })
            .collect();
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(if desk_mic {
            "desk_mic = on\n"
        } else {
            "desk_mic = off\n"
        });
        if updated != text {
            write_file(&path, updated.as_bytes())?;
        }
        Ok(())
    }

    pub fn start_with_windows_default() -> bool {
        if program_dir().is_ok_and(|dir| dir.join("hush.exe").is_file()) {
            RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", KEY_READ)
                .and_then(|run| run.get_value::<String, _>("Hush"))
                .is_ok()
        } else {
            true
        }
    }

    /// Active physical microphones by friendly name (virtual devices from
    /// other products are left out). The microphone Hush is already
    /// attached to, if any, comes first.
    pub fn microphones() -> Vec<Microphone> {
        const MMDEVICES: &str =
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture";
        let Ok(root) =
            RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(MMDEVICES, KEY_READ)
        else {
            return Vec::new();
        };
        let attached =
            fs::read_to_string(data_dir().join(r"Hush\installed.txt")).unwrap_or_default();
        let mut mics: Vec<(bool, Microphone)> = Vec::new();
        for id in root.enum_keys().flatten() {
            let Ok(key) = root.open_subkey_with_flags(&id, KEY_READ) else {
                continue;
            };
            if key.get_value::<u32, _>("DeviceState").unwrap_or(0) != 1 {
                continue;
            }
            let Ok(props) = key.open_subkey_with_flags("Properties", KEY_READ) else {
                continue;
            };
            let short: String = props
                .get_value("{a45c254e-df1c-4efd-8020-67d146a850e0},2")
                .unwrap_or_default();
            let description: String = props
                .get_value("{b3f8fa53-0004-438e-9003-51a46e139bfc},6")
                .unwrap_or_default();
            let name = if description.is_empty() {
                short
            } else {
                format!("{short} ({description})")
            };
            let lower = name.to_lowercase();
            if ["sonar", "vb-audio", "cable", "voicemeeter", "hush"]
                .iter()
                .any(|v| lower.contains(v))
            {
                continue;
            }
            mics.push((
                attached
                    .lines()
                    .any(|line| line.split('\t').nth(1) == Some(&id)),
                Microphone { id, name },
            ));
        }
        mics.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
        mics.into_iter().map(|(_, mic)| mic).collect()
    }

    fn run_logged(log_to: &Log, command: &mut Command) -> Result<(), String> {
        let output = command
            .creation_flags(NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            log(log_to, format!("  {}", line.trim()));
        }
        if output.status.success() {
            Ok(())
        } else {
            Err(text
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("failed")
                .to_string())
        }
    }

    fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        replace_file(path, |file| file.write_all(bytes))
    }

    fn audio(log_to: &Log, start: bool) -> Result<(), String> {
        let (verb, service) = if start {
            ("start", "audiosrv")
        } else {
            ("stop", "audioendpointbuilder")
        };
        let mut command = system_tool("net.exe").map_err(|e| e.to_string())?;
        command.args([verb, service]);
        if !start {
            command.arg("/y");
        }
        run_logged(log_to, &mut command)
    }

    /// Hush was previously called SteelMixer. If that version is installed,
    /// keep the user's settings and remove it cleanly so there are not two
    /// copies. (Old names are spelled out here on purpose.)
    fn migrate_from_steelmixer(log_to: &Log) {
        let Ok(root) = program_files() else { return };
        let old_dir = root.join("SteelMixer");
        let old_setup = old_dir.join("steelmixer-setup.exe");
        if !old_setup.exists() {
            return;
        }
        log(
            log_to,
            "Found SteelMixer (the previous name of Hush): moving your settings",
        );
        let copy_if_missing = |from: PathBuf, to: PathBuf| {
            if from.exists() && !to.exists() {
                if let Some(parent) = to.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::copy(&from, &to);
            }
        };
        let data = data_dir();
        copy_if_missing(
            data.join(r"SteelMixer\mic.conf"),
            data.join(r"Hush\mic.conf"),
        );
        if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
            copy_if_missing(
                appdata.join(r"SteelMixer\app.conf"),
                appdata.join(r"Hush\app.conf"),
            );
        }
        log(log_to, "Removing SteelMixer");
        // The old app holds files in its folder open; close it first.
        let _ = system_tool("taskkill.exe").and_then(|mut command| {
            command
                .args(["/im", "steelmixer.exe", "/f"])
                .creation_flags(NO_WINDOW)
                .output()
        });
        if run_logged(
            log_to,
            Command::new(&old_setup).args(["uninstall-app", "--quiet"]),
        )
        .is_err()
        {
            log(
                log_to,
                "SteelMixer could not be removed. Its installation was left in place.",
            );
            return;
        }
        // The old uninstaller deletes its folder shortly after it exits; make
        // sure nothing is left behind either way.
        std::thread::sleep(std::time::Duration::from_secs(3));
        let _ = fs::remove_dir_all(&old_dir);
        if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
            let _ = fs::remove_dir_all(appdata.join("SteelMixer"));
        }
    }

    /// Installs Hush. `Ok(Some(warning))` means Hush itself is installed
    /// and removable, but part of microphone setup could not finish.
    pub fn run(options: Options, log_to: Log) -> Outcome {
        let dir = program_dir().map_err(|e| e.to_string())?;
        log(&log_to, "Closing Hush if it is running");
        let _ = system_tool("taskkill.exe").and_then(|mut command| {
            command
                .args(["/im", "hush.exe", "/f"])
                .creation_flags(NO_WINDOW)
                .output()
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        migrate_from_steelmixer(&log_to);

        log(&log_to, format!("Copying files to {}", dir.display()));
        fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        write_file(&dir.join("hush.exe"), APP_EXE).map_err(|e| format!("hush.exe: {e}"))?;
        write_file(&dir.join("hush-setup.exe"), SETUP_EXE)
            .map_err(|e| format!("hush-setup.exe: {e}"))?;
        // Both executables are available now. Register recovery before
        // installing the other files or touching audio endpoints.
        log(&log_to, "Registering in Installed apps");
        register_uninstall(&dir)?;
        for (name, text) in [
            ("LICENSE.txt", LICENSE),
            ("THIRD-PARTY-NOTICES.txt", NOTICES),
        ] {
            write_file(&dir.join(name), text.as_bytes()).map_err(|e| format!("{name}: {e}"))?;
        }

        // The plugin DLL may be loaded by Windows audio (upgrade): stage it
        // and let the setup tool swap it in while audio is stopped.
        let attached = match &options.microphone {
            Some(mic) => {
                // Setup executes these files elevated. Keep them under Program
                // Files and refuse to reuse a directory left by another run.
                let staging = dir.join(format!(".setup-{}", std::process::id()));
                fs::create_dir(&staging).map_err(|e| format!("Creating setup staging: {e}"))?;
                let result = (|| {
                    write_file(&staging.join("hush-setup.exe"), SETUP_EXE)
                        .map_err(|e| e.to_string())?;
                    write_file(&staging.join("hush_apo.dll"), APO_DLL)
                        .map_err(|e| e.to_string())?;
                    run_logged(
                        &log_to,
                        Command::new(staging.join("hush-setup.exe")).args([
                            "install",
                            "--mic",
                            mic,
                            "--replace-mic",
                        ]),
                    )
                })();
                let _ = fs::remove_dir_all(&staging);
                result.map_err(|e| format!("Clean Mic setup could not finish: {e}"))
            }
            None => Ok(()),
        };
        // Without a successful attach the DLL is not in place yet; the app's
        // Clean Mic switch installs it from the program folder later.
        if attached.is_err() || options.microphone.is_none() {
            let target = dir.join("hush_apo.dll");
            if write_file(&target, APO_DLL).is_err() {
                let written = audio(&log_to, false).and_then(|()| {
                    write_file(&target, APO_DLL).map_err(|e| format!("hush_apo.dll: {e}"))
                });
                let restarted = audio(&log_to, true);
                match (written, restarted) {
                    (Err(write), Err(restart)) => {
                        return Err(format!("{write}. Windows Audio restart: {restart}"))
                    }
                    (Err(error), _) | (_, Err(error)) => return Err(error),
                    _ => {}
                }
            }
        }
        let mut warning = attached.err();
        if let Some(desk_mic) = options
            .desk_mic
            .filter(|_| warning.is_none() && options.microphone.is_some())
        {
            warning = save_mic_position(desk_mic).err().map(|error| {
                format!("Could not save mic position: {error}. Set it in Hush under Microphone.")
            });
        }
        if let Some(warning) = &warning {
            log(&log_to, warning.clone());
        }

        log(&log_to, "Adding Start menu shortcut");
        let shortcut = data_dir().join(r"Microsoft\Windows\Start Menu\Programs\Hush.lnk");
        let script = "$ErrorActionPreference = 'Stop'; $s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:HUSH_SETUP_SHORTCUT); $s.TargetPath = $env:HUSH_SETUP_TARGET; $s.WorkingDirectory = $env:HUSH_SETUP_DIR; $s.IconLocation = $env:HUSH_SETUP_TARGET + ',0'; $s.Description = 'Hush'; $s.Save()";
        run_logged(
            &log_to,
            system_tool(r"WindowsPowerShell\v1.0\powershell.exe")
                .map_err(|e| e.to_string())?
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("HUSH_SETUP_SHORTCUT", shortcut)
                .env("HUSH_SETUP_TARGET", dir.join("hush.exe"))
                .env("HUSH_SETUP_DIR", &dir),
        )
        .map_err(|e| format!("creating shortcut: {e}"))?;

        let (run, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
            .map_err(|e| e.to_string())?;
        if options.start_with_windows {
            log(&log_to, "Starting Hush with Windows");
            run.set_value(
                "Hush",
                &format!("\"{}\" --minimized", dir.join("hush.exe").display()),
            )
            .map_err(|e| e.to_string())?;
        } else if let Err(error) = run.delete_value("Hush") {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(format!("Removing the start-with-Windows entry: {error}"));
            }
        }
        log(&log_to, "Done");
        Ok(warning)
    }

    fn register_uninstall(dir: &Path) -> Result<(), String> {
        let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .create_subkey(UNINSTALL_KEY)
            .map_err(|e| e.to_string())?;
        let setup = dir.join("hush-setup.exe");
        let size_kb = ((APP_EXE.len() + SETUP_EXE.len() + APO_DLL.len()) / 1024) as u32;
        let values: [(&str, String); 8] = [
            ("DisplayName", "Hush".into()),
            ("DisplayVersion", VERSION.into()),
            ("Publisher", "Exel".into()),
            ("DisplayIcon", dir.join("hush.exe").display().to_string()),
            ("InstallLocation", dir.display().to_string()),
            ("URLInfoAbout", HOMEPAGE.into()),
            (
                "UninstallString",
                format!("\"{}\" --uninstall", dir.join("hush.exe").display()),
            ),
            (
                "QuietUninstallString",
                format!("\"{}\" uninstall-app --quiet", setup.display()),
            ),
        ];
        for (name, value) in values {
            if value.is_empty() {
                continue;
            }
            key.set_value(name, &value).map_err(|e| e.to_string())?;
        }
        for (name, value) in [
            ("NoModify", 1_u32),
            ("NoRepair", 1),
            ("EstimatedSize", size_kb),
        ] {
            key.set_value(name, &value).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Starts the installed app as the signed-in user (not elevated), by
    /// asking Explorer to open it.
    pub fn launch_app() {
        if let (Ok(mut explorer), Ok(dir)) = (system_tool(r"..\explorer.exe"), program_dir()) {
            let _ = explorer.arg(dir.join("hush.exe")).spawn();
        }
    }
}

#[cfg(windows)]
mod wizard {
    use crate::{icon_art, install, theme};
    use eframe::egui::{self, RichText, TextureHandle};
    use std::sync::{Arc, Mutex};

    #[derive(PartialEq, Clone, Copy)]
    enum Step {
        Welcome,
        Options,
        Installing,
        Done,
    }

    pub struct Wizard {
        step: Step,
        logo: TextureHandle,
        mics: Vec<install::Microphone>,
        mic_index: usize,
        attach: bool,
        desk_mic: Option<bool>,
        has_saved_mic_settings: bool,
        start_with_windows: bool,
        launch: bool,
        log: install::Log,
        result: Arc<Mutex<Option<install::Outcome>>>,
    }

    impl Wizard {
        pub fn new(ctx: &egui::Context) -> Self {
            theme::install(ctx);
            let logo = ctx.load_texture(
                "logo",
                egui::ColorImage::from_rgba_unmultiplied([96, 96], &icon_art::render_icon(96)),
                egui::TextureOptions::LINEAR,
            );
            let mics = install::microphones();
            let has_saved_mic_settings = install::has_saved_mic_settings();
            Self {
                // Developer aid for screenshots: HUSH_SETUP_STEP=options.
                step: if std::env::var("HUSH_SETUP_STEP").as_deref() == Ok("options") {
                    Step::Options
                } else {
                    Step::Welcome
                },
                logo,
                attach: !mics.is_empty(),
                mics,
                mic_index: 0,
                desk_mic: (!has_saved_mic_settings).then_some(true),
                has_saved_mic_settings,
                start_with_windows: install::start_with_windows_default(),
                launch: true,
                log: Arc::new(Mutex::new(Vec::new())),
                result: Arc::new(Mutex::new(None)),
            }
        }

        fn start_install(&mut self, ctx: &egui::Context) {
            self.step = Step::Installing;
            let options = install::Options {
                microphone: if self.attach {
                    self.mics.get(self.mic_index).map(|mic| mic.id.clone())
                } else {
                    None
                },
                desk_mic: self.desk_mic,
                start_with_windows: self.start_with_windows,
            };
            let (log, result, ctx) = (self.log.clone(), self.result.clone(), ctx.clone());
            std::thread::spawn(move || {
                let outcome = install::run(options, log);
                *result.lock().unwrap() = Some(outcome);
                ctx.request_repaint();
            });
        }

        fn header(&self, ui: &mut egui::Ui, title: &str, subtitle: &str) {
            let p = theme::palette(ui);
            ui.horizontal(|ui| {
                ui.add(egui::Image::new(&self.logo).fit_to_exact_size(egui::vec2(44.0, 44.0)));
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(title).heading());
                    ui.label(RichText::new(subtitle).color(p.muted_foreground));
                });
            });
            ui.add_space(16.0);
        }

        fn footer(&mut self, ui: &mut egui::Ui) {
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| match self.step {
                            Step::Welcome => {
                                if theme::primary_button(ui, "Next").clicked() {
                                    self.step = Step::Options;
                                }
                                if theme::outline_button(ui, "Cancel").clicked() {
                                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            }
                            Step::Options => {
                                if theme::primary_button(ui, "Install").clicked() {
                                    let ctx = ui.ctx().clone();
                                    self.start_install(&ctx);
                                }
                                if theme::outline_button(ui, "Back").clicked() {
                                    self.step = Step::Welcome;
                                }
                            }
                            Step::Installing => {
                                ui.spinner();
                            }
                            Step::Done => {
                                let ok = matches!(*self.result.lock().unwrap(), Some(Ok(_)));
                                if theme::primary_button(ui, "Finish").clicked() {
                                    if ok && self.launch {
                                        install::launch_app();
                                    }
                                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            }
                        },
                    );
                });
            });
        }
    }

    impl eframe::App for Wizard {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            if self.step == Step::Installing && self.result.lock().unwrap().is_some() {
                self.step = Step::Done;
            }
            let p = theme::palette(ui);
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(p.background)
                        .inner_margin(egui::Margin::same(28)),
                )
                .show(ui, |ui| {
                    egui::Panel::bottom("footer")
                        .frame(
                            egui::Frame::new()
                                .fill(p.background)
                                .inner_margin(egui::Margin {
                                    left: 0,
                                    right: 0,
                                    top: 14,
                                    bottom: 0,
                                }),
                        )
                        .show(ui, |ui| self.footer(ui));
                    egui::CentralPanel::default()
                        .frame(egui::Frame::new().fill(p.background))
                        .show(ui, |ui| match self.step {
                            Step::Welcome => self.welcome(ui),
                            Step::Options => self.options(ui),
                            Step::Installing | Step::Done => self.progress(ui),
                        });
                });
            if self.step == Step::Installing {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }

    impl Wizard {
        fn welcome(&mut self, ui: &mut egui::Ui) {
            self.header(ui, "Install Hush", &format!("Version {}", install::VERSION));
            theme::card(ui, None, None, |ui| {
                ui.label(theme::semibold_text("A clean microphone in every app"));
                theme::hint(ui, "AI noise removal, a voice gate that closes your mic between words, and voice EQ, applied inside Windows' own audio engine.");
                ui.add_space(6.0);
                theme::hint(ui, "No virtual devices or drivers are installed. Discord, games and other apps keep using your microphone as usual.");
                ui.add_space(6.0);
                theme::hint(ui, "Setup briefly restarts Windows audio. You can remove Hush at any time from Settings > Apps.");
            });
        }

        fn options(&mut self, ui: &mut egui::Ui) {
            self.header(ui, "Options", "You can change all of these later in Hush.");
            theme::card(ui, None, None, |ui| {
                if self.mics.is_empty() {
                    theme::hint(ui, "No microphone found. You can turn on Clean Mic later from the Hush window.");
                    self.attach = false;
                } else {
                    theme::row(
                        ui,
                        "Clean up my microphone",
                        "Install Clean Mic on the microphone below.",
                        |ui| {
                            theme::switch(ui, &mut self.attach, "Clean up my microphone");
                        },
                    );
                    ui.add_enabled_ui(self.attach, |ui| {
                        egui::ComboBox::from_id_salt("mic")
                            .width(ui.available_width())
                            .selected_text(&self.mics[self.mic_index].name)
                            .show_ui(ui, |ui| {
                                for (index, mic) in self.mics.iter().enumerate() {
                                    ui.selectable_value(&mut self.mic_index, index, &mic.name);
                                }
                            });
                        ui.add_space(6.0);
                        theme::row_sized(
                            ui,
                            "Mic position",
                            "Choose how far the mic sits from your mouth.",
                            theme::MIC_POSITION_WIDTH,
                            |ui| {
                                theme::mic_position(
                                    ui,
                                    &mut self.desk_mic,
                                    self.has_saved_mic_settings,
                                )
                            },
                        );
                    });
                }
                theme::separator(ui);
                theme::row(
                    ui,
                    "Start with Windows",
                    "Runs quietly in the system tray.",
                    |ui| {
                        theme::switch(ui, &mut self.start_with_windows, "Start with Windows");
                    },
                );
                theme::separator(ui);
                theme::row(ui, "Open Hush when done", "", |ui| {
                    theme::switch(ui, &mut self.launch, "Open Hush when done");
                });
            });
        }

        fn progress(&mut self, ui: &mut egui::Ui) {
            let result = self.result.lock().unwrap().clone();
            let (title, subtitle) = match &result {
                None => (
                    "Installing…",
                    "This takes a few seconds. Windows audio restarts briefly.",
                ),
                Some(Ok(None)) => (
                    "Hush is installed",
                    "Find it in the Start menu and the system tray.",
                ),
                Some(Ok(Some(_))) => (
                    "Hush is installed with an audio setup warning",
                    "Review the details below, then check the microphone in Hush.",
                ),
                Some(Err(_)) => (
                    "Installation failed",
                    "Run Setup again, or remove Hush from Settings > Apps.",
                ),
            };
            self.header(ui, title, subtitle);
            let p = theme::palette(ui);
            theme::card(ui, None, None, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in self.log.lock().unwrap().iter() {
                            ui.label(RichText::new(line).small().color(p.muted_foreground));
                        }
                        if let Some(Err(error) | Ok(Some(error))) = &result {
                            ui.label(RichText::new(error).color(p.destructive));
                        }
                    });
            });
            if matches!(result, Some(Ok(None))) {
                ui.add_space(10.0);
                theme::hint(ui, "In Discord and games, select your microphone directly. In Hush, open Mic Test to hear the difference.");
            }
        }
    }
}
