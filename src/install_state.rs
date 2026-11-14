//! What the Hush window shows about the installed APO: which
//! microphone it is attached to and whether the audio engine is running it.
//! Read-only; installing/removing is delegated to `hush-setup.exe`.

use hush_apo::settings;
use std::{path::PathBuf, process::Command, sync::mpsc, time::SystemTime};
use winreg::{
    enums::{HKEY_LOCAL_MACHINE, KEY_READ},
    RegKey,
};

const MMDEVICES: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio";
const PKEY_NAME: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
const PKEY_DESCRIPTION: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";

#[derive(Debug, Clone, Default)]
pub struct InstallState {
    /// Friendly names of microphones the APO is attached to.
    pub attached: Vec<String>,
    /// The APO reported processing within the last few seconds.
    pub running: bool,
    /// Late frames (short audio gaps) reported by the APO since it started.
    pub glitches: u64,
}

pub fn endpoint_name(flow: &str, id: &str) -> Option<String> {
    let props = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"{MMDEVICES}\{flow}\{id}\Properties"), KEY_READ)
        .ok()?;
    let short: String = props.get_value(PKEY_NAME).ok()?;
    let description: String = props.get_value(PKEY_DESCRIPTION).unwrap_or_default();
    Some(if description.is_empty() {
        short
    } else {
        format!("{short} ({description})")
    })
}

/// Ids (GUIDs) of active capture endpoints whose friendly name is `name`.
pub fn active_capture_ids(name: &str) -> Vec<String> {
    let Ok(capture) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"{MMDEVICES}\Capture"), KEY_READ)
    else {
        return Vec::new();
    };
    capture
        .enum_keys()
        .flatten()
        .filter(|id| {
            let active = capture
                .open_subkey_with_flags(id, KEY_READ)
                .and_then(|key| key.get_value::<u32, _>("DeviceState"))
                .is_ok_and(|state| state == 1);
            active
                && endpoint_name("Capture", id)
                    .is_some_and(|found| found.trim().eq_ignore_ascii_case(name.trim()))
        })
        .collect()
}

pub fn read() -> InstallState {
    let record =
        std::fs::read_to_string(settings::settings_dir().join("installed.txt")).unwrap_or_default();
    let mut attached: Vec<String> = Vec::new();
    for line in record.lines() {
        let mut parts = line.split('\t');
        let (Some(flow), Some(id)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = endpoint_name(flow, id).unwrap_or_else(|| id.to_string());
        if !attached.contains(&name) {
            attached.push(name);
        }
    }
    let status = std::fs::read_to_string(settings::status_path())
        .ok()
        .and_then(|text| settings::parse_status(&text));
    let running = status.as_ref().is_some_and(|status| {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(status.updated_unix) <= 3 && status.processed_frames > 0
    });
    InstallState {
        attached,
        running,
        glitches: status.map(|s| s.underruns).unwrap_or(0),
    }
}

fn setup_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("hush-setup.exe")))
        .unwrap_or_else(|| PathBuf::from("hush-setup.exe"))
}

/// Runs `hush-setup.exe` (which asks Windows for admin permission) on
/// a background thread; the result text arrives on the returned channel.
pub fn run_setup(args: Vec<String>) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let text = match Command::new(setup_exe())
            .args(&args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                if output.status.success() {
                    "Done.".to_string()
                } else {
                    let last = text
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("cancelled")
                        .to_string();
                    format!("Not completed: {last}")
                }
            }
            Err(error) => format!("Could not run hush-setup.exe: {error}"),
        };
        let _ = sender.send(text);
    });
    receiver
}
