//! "Copy diagnostics": a plain-text summary users can paste into a bug
//! report. It contains no audio and no personal data beyond the names of
//! the microphones, and it is only built when the user clicks the button.

use hush_apo::{
    dsp::MicSettings,
    settings::{self, EngineStatus},
};
use std::fmt::Write as _;
use winreg::{
    enums::{HKEY_LOCAL_MACHINE, KEY_READ},
    RegKey,
};

pub struct Report<'a> {
    pub version: &'a str,
    pub windows: &'a str,
    pub selected_mic: &'a str,
    pub attached: &'a [String],
    pub processing: bool,
    pub status: Option<&'a EngineStatus>,
    pub now_unix: u64,
    pub glitches: u64,
    pub settings: &'a MicSettings,
    pub mute_hotkey: Option<&'a str>,
    pub muted: bool,
    pub health: &'a str,
    /// Last `repair.txt` result, if any.
    pub last_repair: Option<&'a str>,
    pub repair_tasks: &'a str,
}

pub fn text(report: &Report) -> String {
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let mut out = String::from("Hush diagnostics\n");
    let _ = writeln!(out, "Hush version: {}", report.version);
    let _ = writeln!(out, "Windows: {}", report.windows);
    let _ = writeln!(
        out,
        "Selected microphone: {}",
        if report.selected_mic.is_empty() {
            "none"
        } else {
            report.selected_mic
        }
    );
    let _ = writeln!(
        out,
        "Clean Mic installed on: {}",
        if report.attached.is_empty() {
            "none".to_string()
        } else {
            report.attached.join("; ")
        }
    );
    let _ = writeln!(out, "Engine processing: {}", yes_no(report.processing));
    match report.status {
        Some(status) => {
            let _ = writeln!(out, "Engine sample rate: {} Hz", status.rate);
            let _ = writeln!(out, "Engine channels: {}", status.channels);
            let _ = writeln!(out, "Engine underruns: {}", status.underruns);
            let _ = writeln!(out, "Engine skipped samples: {}", status.skipped);
            let _ = writeln!(
                out,
                "Engine last update: {} s ago",
                report.now_unix.saturating_sub(status.updated_unix)
            );
        }
        None => {
            let _ = writeln!(out, "Engine status: not available");
        }
    }
    let _ = writeln!(
        out,
        "Audio gaps since Windows audio started: {}",
        report.glitches
    );
    match report.mute_hotkey {
        Some(key) => {
            let _ = writeln!(out, "Mute hotkey: on ({key})");
        }
        None => {
            let _ = writeln!(out, "Mute hotkey: off");
        }
    }
    let _ = writeln!(out, "Microphone muted: {}", yes_no(report.muted));
    let _ = writeln!(out, "Microphone health: {}", report.health);
    let _ = writeln!(
        out,
        "Last repair: {}",
        report.last_repair.unwrap_or("none")
    );
    let _ = writeln!(out, "Repair tasks: {}", report.repair_tasks);
    out.push_str("Settings:\n");
    out.push_str(&settings::format(report.settings));
    out
}

/// "Windows 11 Pro 24H2 (build 26100.1234)" from the registry.
pub fn windows_version() -> String {
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", KEY_READ)
    else {
        return "unknown".into();
    };
    let get = |name: &str| key.get_value::<String, _>(name).unwrap_or_default();
    let build = get("CurrentBuildNumber");
    let revision = key.get_value::<u32, _>("UBR").ok();
    format_windows_version(&get("ProductName"), &get("DisplayVersion"), &build, revision)
}

fn format_windows_version(
    product: &str,
    display_version: &str,
    build: &str,
    revision: Option<u32>,
) -> String {
    let mut name = product.trim().to_string();
    // The registry still says "Windows 10" on Windows 11.
    if build.trim().parse::<u32>().is_ok_and(|b| b >= 22000) {
        name = name.replacen("Windows 10", "Windows 11", 1);
        if !name.contains("Windows 11") {
            name = "Windows 11".into();
        }
    }
    if name.is_empty() {
        name = "Windows".into();
    }
    let mut out = name;
    if !display_version.trim().is_empty() {
        let _ = write!(out, " {}", display_version.trim());
    }
    if !build.trim().is_empty() {
        match revision {
            Some(ubr) => {
                let _ = write!(out, " (build {}.{ubr})", build.trim());
            }
            None => {
                let _ = write!(out, " (build {})", build.trim());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_version_and_settings_block() {
        let settings = MicSettings::default();
        let attached = vec!["Microphone (USB)".to_string()];
        let report = Report {
            version: "9.9.9",
            windows: "Windows 11 (build 26100)",
            selected_mic: "Microphone (USB)",
            attached: &attached,
            processing: true,
            status: None,
            now_unix: 100,
            glitches: 3,
            settings: &settings,
            mute_hotkey: Some("Ctrl+Alt+M"),
            muted: false,
            health: "Healthy",
            last_repair: Some("repaired: done"),
            repair_tasks: "repair yes, restart yes",
        };
        let text = text(&report);
        assert!(text.starts_with("Hush diagnostics\n"));
        assert!(text.contains("Hush version: 9.9.9"));
        assert!(text.contains("Mute hotkey: on (Ctrl+Alt+M)"));
        assert!(text.contains("Microphone health: Healthy"));
        assert!(text.contains("Last repair: repaired: done"));
        assert!(text.contains(&format!("Settings:\n{}", settings::format(&settings))));
    }

    #[test]
    fn windows_11_is_detected_from_the_build_number() {
        assert_eq!(
            format_windows_version("Windows 10 Pro", "24H2", "26100", Some(1234)),
            "Windows 11 Pro 24H2 (build 26100.1234)"
        );
        assert_eq!(
            format_windows_version("Windows 10 Home", "22H2", "19045", None),
            "Windows 10 Home 22H2 (build 19045)"
        );
    }
}
