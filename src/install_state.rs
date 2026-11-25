//! What the Hush window shows about the installed APO: which
//! microphone it is attached to and whether the audio engine is running it.
//! Read-only; installing/removing is delegated to `hush-setup.exe`.

use crate::endpoints;
use hush_apo::settings;
use std::{path::PathBuf, process::Command, sync::mpsc, time::SystemTime};

#[derive(Debug, Clone, Default)]
pub struct InstallState {
    /// Friendly names of microphones the APO is attached to.
    pub attached: Vec<String>,
    /// The APO reported processing within the last few seconds.
    pub running: bool,
    /// Late frames (short audio gaps) reported by the APO since it started.
    pub glitches: u64,
    /// Clean Mic's own CPU use while processing, in tenths of a percent of
    /// one core (as the plugin measured it over the last second).
    pub cpu_permille: Option<u32>,
}

/// Endpoints recorded by `hush-setup` as (flow, id), without duplicates.
pub fn recorded_endpoints() -> Vec<(String, String)> {
    let record =
        std::fs::read_to_string(settings::settings_dir().join("installed.txt")).unwrap_or_default();
    let mut list: Vec<(String, String)> = Vec::new();
    for line in record.lines() {
        let mut parts = line.split('\t');
        if let (Some(flow), Some(id)) = (parts.next(), parts.next()) {
            let entry = (flow.to_string(), id.to_string());
            if !list.contains(&entry) {
                list.push(entry);
            }
        }
    }
    list
}

pub fn read() -> InstallState {
    let mut attached: Vec<String> = Vec::new();
    for (flow, id) in recorded_endpoints() {
        let name = endpoints::endpoint_name(&flow, &id).unwrap_or(id);
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
        glitches: status.as_ref().map(|s| s.underruns).unwrap_or(0),
        cpu_permille: status.and_then(|s| s.cpu_permille).filter(|_| running),
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
