//! App preferences (per user) and "Start with Windows".
//!
//! Microphone processing settings live in %ProgramData%\Hush\mic.conf
//! (shared with the audio engine). These are only about the app itself and
//! live in %APPDATA%\Hush\app.conf.

use std::path::PathBuf;
use winreg::{
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE},
    RegKey,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Hush";
/// Command-line flag used by the autostart entry.
pub const MINIMIZED_FLAG: &str = "--minimized";

/// Keys offered for the global hotkeys (mute, Clean Mic on/off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MuteKey {
    #[default]
    CtrlAltM,
    CtrlShiftM,
    Pause,
    ScrollLock,
    F13,
}

impl MuteKey {
    pub const ALL: [MuteKey; 5] = [
        MuteKey::CtrlAltM,
        MuteKey::CtrlShiftM,
        MuteKey::Pause,
        MuteKey::ScrollLock,
        MuteKey::F13,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MuteKey::CtrlAltM => "Ctrl+Alt+M",
            MuteKey::CtrlShiftM => "Ctrl+Shift+M",
            MuteKey::Pause => "Pause",
            MuteKey::ScrollLock => "Scroll Lock",
            MuteKey::F13 => "F13",
        }
    }

    /// Name stored in app.conf.
    fn id(self) -> &'static str {
        match self {
            MuteKey::CtrlAltM => "ctrl+alt+m",
            MuteKey::CtrlShiftM => "ctrl+shift+m",
            MuteKey::Pause => "pause",
            MuteKey::ScrollLock => "scrolllock",
            MuteKey::F13 => "f13",
        }
    }

    fn from_id(id: &str) -> Option<MuteKey> {
        let id = id.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|key| key.id() == id)
    }

    /// Windows virtual-key code.
    pub fn virtual_key(self) -> u32 {
        match self {
            MuteKey::CtrlAltM | MuteKey::CtrlShiftM => 0x4D, // 'M'
            MuteKey::Pause => 0x13,                          // VK_PAUSE
            MuteKey::ScrollLock => 0x91,                     // VK_SCROLL
            MuteKey::F13 => 0x7C,                            // VK_F13
        }
    }

    /// RegisterHotKey modifier bits (MOD_ALT = 1, MOD_CONTROL = 2, MOD_SHIFT = 4).
    pub fn modifiers(self) -> u32 {
        match self {
            MuteKey::CtrlAltM => 1 | 2,
            MuteKey::CtrlShiftM => 2 | 4,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prefs {
    /// Closing the window keeps Hush running in the tray.
    pub close_to_tray: bool,
    /// Show a one-time hint the first time the window is closed to the tray.
    pub tray_hint_shown: bool,
    /// Global hotkey that mutes the microphone in every app.
    pub mute_hotkey: bool,
    pub mute_key: MuteKey,
    /// Global hotkey that turns Clean Mic on and off (off by default).
    pub toggle_hotkey: bool,
    pub toggle_key: MuteKey,
    /// Put Clean Mic back automatically when a microphone reconnects without it.
    pub auto_repair: bool,
    /// Windows notifications about microphone health.
    pub notifications: bool,
    /// Where the window was when it last closed.
    pub window: Option<WindowRect>,
}

/// Window position and size in points, plus the pixels per point at the
/// time (to check the position is still on a screen).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub pixels_per_point: f32,
}

impl WindowRect {
    fn id(&self) -> String {
        format!(
            "{},{},{},{},{}",
            self.x, self.y, self.width, self.height, self.pixels_per_point
        )
    }

    fn from_id(text: &str) -> Option<WindowRect> {
        let values: Vec<f32> = text
            .split(',')
            .map(|v| v.trim().parse::<f32>())
            .collect::<Result<_, _>>()
            .ok()?;
        let [x, y, width, height, pixels_per_point] = values[..] else {
            return None;
        };
        let sane = values.iter().all(|v| v.is_finite())
            && (200.0..=10_000.0).contains(&width)
            && (200.0..=10_000.0).contains(&height)
            && (0.5..=8.0).contains(&pixels_per_point);
        sane.then_some(WindowRect {
            x,
            y,
            width,
            height,
            pixels_per_point,
        })
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            tray_hint_shown: false,
            mute_hotkey: false,
            mute_key: MuteKey::default(),
            toggle_hotkey: false,
            toggle_key: MuteKey::CtrlShiftM,
            auto_repair: true,
            notifications: true,
            window: None,
        }
    }
}

/// `%APPDATA%\Hush`: per-user app files.
pub fn dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Hush")
}

pub fn path() -> PathBuf {
    dir().join("app.conf")
}

fn flag(value: &str) -> Option<bool> {
    match value.trim() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

pub fn load() -> Prefs {
    try_load().unwrap_or_default()
}

pub fn try_load() -> std::io::Result<Prefs> {
    let mut prefs = Prefs::default();
    let text = match hush_apo::settings::read_config(&path()) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(prefs),
        Err(error) => return Err(error),
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "close_to_tray" => prefs.close_to_tray = flag(value).unwrap_or(prefs.close_to_tray),
            "tray_hint_shown" => {
                prefs.tray_hint_shown = flag(value).unwrap_or(prefs.tray_hint_shown)
            }
            "mute_hotkey" => prefs.mute_hotkey = flag(value).unwrap_or(prefs.mute_hotkey),
            "auto_repair" => prefs.auto_repair = flag(value).unwrap_or(prefs.auto_repair),
            "notifications" => prefs.notifications = flag(value).unwrap_or(prefs.notifications),
            "mute_key" => prefs.mute_key = MuteKey::from_id(value).unwrap_or(prefs.mute_key),
            "window" => prefs.window = WindowRect::from_id(value),
            "clean_mic_hotkey" => prefs.toggle_hotkey = flag(value).unwrap_or(prefs.toggle_hotkey),
            "clean_mic_key" => {
                prefs.toggle_key = MuteKey::from_id(value).unwrap_or(prefs.toggle_key)
            }
            _ => {}
        }
    }
    Ok(prefs)
}

pub fn save(prefs: &Prefs) -> std::io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let on = |b: bool| if b { "on" } else { "off" };
    let mut text = format!(
        "close_to_tray = {}\ntray_hint_shown = {}\nmute_hotkey = {}\nmute_key = {}\nauto_repair = {}\nnotifications = {}\n",
        on(prefs.close_to_tray),
        on(prefs.tray_hint_shown),
        on(prefs.mute_hotkey),
        prefs.mute_key.id(),
        on(prefs.auto_repair),
        on(prefs.notifications)
    );
    text.push_str(&format!(
        "clean_mic_hotkey = {}\nclean_mic_key = {}\n",
        on(prefs.toggle_hotkey),
        prefs.toggle_key.id()
    ));
    if let Some(window) = prefs.window {
        text.push_str(&format!("window = {}\n", window.id()));
    }
    hush_apo::settings::write_atomic(&path, &text)
}

pub fn initialize_startup() {
    if !crate::notify::running_installed() {
        return;
    }
    let user = RegKey::predef(HKEY_CURRENT_USER);
    let Ok((key, _)) = user.create_subkey(r"Software\Exel\Hush") else {
        return;
    };
    if key.get_value::<u32, _>("StartupInitialized").is_ok() {
        return;
    }
    // Preserve existing per-user decisions on upgrade.
    let existing = start_with_windows()
        || user
            .open_subkey(r"Software\Classes\AppUserModelId\Exel.Hush")
            .is_ok();
    if !existing
        && hush::machine_state::read("StartOnFirstLaunch").is_ok_and(|value| value == "1")
        && set_start_with_windows(true).is_err()
    {
        return;
    }
    let _ = key.set_value("StartupInitialized", &1_u32);
}

pub fn cleanup_install() {
    let _ = set_start_with_windows(false);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for key in [
        r"Software\Classes\hush",
        r"Software\Classes\AppUserModelId\Exel.Hush",
        r"Software\Exel\Hush",
    ] {
        let _ = hkcu.delete_subkey_all(key);
    }
    let _ = std::fs::remove_dir_all(dir());
}

/// True if Hush registered a sign-in request for this user.
pub fn start_with_windows() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|key| key.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

/// Adds or removes the per-user autostart entry (no admin rights needed).
/// The entry starts Hush hidden in the tray.
pub fn set_start_with_windows(enabled: bool) -> std::io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_SET_VALUE)
        .unwrap_or(key);
    if enabled {
        let exe = std::env::current_exe()?;
        key.set_value(
            RUN_VALUE,
            &format!("\"{}\" {MINIMIZED_FLAG}", exe.display()),
        )
    } else {
        match key.delete_value(RUN_VALUE) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_rect_round_trips_and_rejects_nonsense() {
        let rect = WindowRect {
            x: -1200.5,
            y: 80.0,
            width: 900.0,
            height: 720.0,
            pixels_per_point: 1.5,
        };
        assert_eq!(WindowRect::from_id(&rect.id()), Some(rect));
        assert_eq!(WindowRect::from_id("1,2,3"), None);
        assert_eq!(WindowRect::from_id("0,0,50,50,1"), None);
        assert_eq!(WindowRect::from_id("0,0,900,700,NaN"), None);
        assert_eq!(WindowRect::from_id("garbage"), None);
    }
}
