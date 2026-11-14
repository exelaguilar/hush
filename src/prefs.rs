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

/// Keys offered for the global mute hotkey.
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
    /// Put Clean Mic back automatically when a microphone reconnects without it.
    pub auto_repair: bool,
    /// Windows notifications about microphone health.
    pub notifications: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            tray_hint_shown: false,
            mute_hotkey: false,
            mute_key: MuteKey::default(),
            auto_repair: true,
            notifications: true,
        }
    }
}

fn path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Hush")
        .join("app.conf")
}

fn flag(value: &str) -> Option<bool> {
    match value.trim() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

pub fn load() -> Prefs {
    let mut prefs = Prefs::default();
    let text = std::fs::read_to_string(path()).unwrap_or_default();
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
            _ => {}
        }
    }
    prefs
}

pub fn save(prefs: &Prefs) -> std::io::Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let on = |b: bool| if b { "on" } else { "off" };
    std::fs::write(
        path,
        format!(
            "close_to_tray = {}\ntray_hint_shown = {}\nmute_hotkey = {}\nmute_key = {}\nauto_repair = {}\nnotifications = {}\n",
            on(prefs.close_to_tray),
            on(prefs.tray_hint_shown),
            on(prefs.mute_hotkey),
            prefs.mute_key.id(),
            on(prefs.auto_repair),
            on(prefs.notifications)
        ),
    )
}

/// True if Hush starts at sign-in for this user.
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
