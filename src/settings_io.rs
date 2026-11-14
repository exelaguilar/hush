//! Saving, restoring, exporting and importing microphone settings.
//!
//! The tuning snapshot is one slot in %APPDATA%\Hush. Export/import use the
//! standard Windows file dialogs, shown on their own thread so the window
//! never freezes. Imported or restored settings never change `enabled`; the
//! caller keeps the current Clean Mic on/off state.

use hush_apo::{dsp::MicSettings, settings};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::SystemTime,
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        },
        UI::Shell::{
            Common::COMDLG_FILTERSPEC, FileOpenDialog, FileSaveDialog, IFileDialog,
            IFileOpenDialog, IFileSaveDialog, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM,
            FOS_OVERWRITEPROMPT, SIGDN_FILESYSPATH,
        },
    },
};
use windows::core::Interface;

const MAX_IMPORT_BYTES: u64 = 64 * 1024;

// -- Parsing ---------------------------------------------------------------------

/// Keys that `settings::format` writes.
fn known_keys() -> Vec<String> {
    settings::format(&MicSettings::default())
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, _)| key.trim().to_string())
        .collect()
}

/// Parses a settings file, refusing text that has no recognised key
/// (`settings::parse` alone would silently return defaults).
pub fn parse_settings_text(text: &str) -> Result<MicSettings, String> {
    let known = known_keys();
    let recognised = text.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("");
        line.split_once('=')
            .is_some_and(|(key, _)| known.iter().any(|k| k == key.trim()))
    });
    if recognised {
        Ok(settings::parse(text))
    } else {
        Err("That file does not look like Hush settings.".into())
    }
}

/// Reads and validates a settings file (size limit, known keys).
pub fn read_settings_file(path: &Path) -> Result<MicSettings, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("Could not read the file: {e}"))?
        .len();
    if size > MAX_IMPORT_BYTES {
        return Err("That file is too large to be Hush settings.".into());
    }
    let text = std::fs::read_to_string(path).map_err(|_| "That file is not text.".to_string())?;
    parse_settings_text(&text)
}

// -- Tuning snapshot ---------------------------------------------------------------

fn snapshot_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Hush")
        .join("tuning-snapshot.conf")
}

pub fn save_snapshot(current: &MicSettings) -> std::io::Result<()> {
    let path = snapshot_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, settings::format(current))
}

/// When the snapshot was saved, if one exists.
pub fn snapshot_time() -> Option<SystemTime> {
    std::fs::metadata(snapshot_path()).ok()?.modified().ok()
}

pub fn load_snapshot() -> Result<MicSettings, String> {
    read_settings_file(&snapshot_path())
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn ago(seconds: u64) -> String {
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86399 => format!("{} h ago", seconds / 3600),
        _ => {
            let days = seconds / 86400;
            format!("{days} day{} ago", if days == 1 { "" } else { "s" })
        }
    }
}

// -- File dialogs ------------------------------------------------------------------

pub enum DialogResult {
    /// Settings were written to this file.
    Exported(PathBuf),
    Imported(MicSettings),
    Failed(String),
}

#[derive(Clone, Copy)]
enum Kind {
    Export,
    Import,
}

pub fn export(current: MicSettings, wake: impl Fn() + Send + 'static) -> mpsc::Receiver<DialogResult> {
    run_dialog(Kind::Export, Some(current), wake)
}

pub fn import(wake: impl Fn() + Send + 'static) -> mpsc::Receiver<DialogResult> {
    run_dialog(Kind::Import, None, wake)
}

fn run_dialog(
    kind: Kind,
    current: Option<MicSettings>,
    wake: impl Fn() + Send + 'static,
) -> mpsc::Receiver<DialogResult> {
    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("hush-file-dialog".into())
        .spawn({
            let sender = sender.clone();
            move || {
                let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
                let picked = pick(kind);
                if com {
                    unsafe { CoUninitialize() };
                }
                let result = match picked {
                    Ok(None) => None, // cancelled
                    Ok(Some(path)) => Some(match (kind, current) {
                        (Kind::Export, Some(current)) => {
                            match std::fs::write(&path, settings::format(&current)) {
                                Ok(()) => DialogResult::Exported(path),
                                Err(e) => DialogResult::Failed(format!("Could not save: {e}")),
                            }
                        }
                        _ => match read_settings_file(&path) {
                            Ok(imported) => DialogResult::Imported(imported),
                            Err(message) => DialogResult::Failed(message),
                        },
                    }),
                    Err(message) => Some(DialogResult::Failed(message)),
                };
                // A cancelled dialog just drops the channel.
                if let Some(result) = result {
                    let _ = sender.send(result);
                }
                wake();
            }
        });
    if spawned.is_err() {
        let _ = sender.send(DialogResult::Failed("Could not open the file dialog.".into()));
    }
    receiver
}

/// Shows the dialog. `Ok(None)` means the user cancelled.
fn pick(kind: Kind) -> Result<Option<PathBuf>, String> {
    let failed = |e: windows::core::Error| e.message();
    let filter = [
        COMDLG_FILTERSPEC {
            pszName: w!("Hush settings (*.conf)"),
            pszSpec: w!("*.conf"),
        },
        COMDLG_FILTERSPEC {
            pszName: w!("All files (*.*)"),
            pszSpec: w!("*.*"),
        },
    ];
    let dialog: IFileDialog = match kind {
        Kind::Export => {
            let d: IFileSaveDialog =
                unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) }
                    .map_err(failed)?;
            unsafe {
                d.SetFileName(w!("hush-settings.conf")).map_err(failed)?;
                d.SetOptions(FOS_FORCEFILESYSTEM | FOS_OVERWRITEPROMPT)
                    .map_err(failed)?;
            }
            d.cast().map_err(failed)?
        }
        Kind::Import => {
            let d: IFileOpenDialog =
                unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
                    .map_err(failed)?;
            unsafe {
                d.SetOptions(FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST)
                    .map_err(failed)?;
            }
            d.cast().map_err(failed)?
        }
    };
    unsafe {
        dialog.SetFileTypes(&filter).map_err(failed)?;
        dialog.SetDefaultExtension(w!("conf")).map_err(failed)?;
        dialog.SetTitle(match kind {
            Kind::Export => w!("Export Hush settings"),
            Kind::Import => w!("Import Hush settings"),
        })
        .map_err(failed)?;
        // Cancelling returns an error (ERROR_CANCELLED).
        if dialog.Show(None).is_err() {
            return Ok(None);
        }
        let item = dialog.GetResult().map_err(failed)?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(failed)?;
        let path = PCWSTR(name.0).to_string().map_err(|e| e.to_string());
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(|p| Some(PathBuf::from(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_text_round_trips() {
        let mut s = MicSettings::default();
        s.gain_db = 3.5;
        s.gate = !s.gate;
        let parsed = parse_settings_text(&settings::format(&s)).unwrap();
        assert_eq!(parsed.gain_db, 3.5);
        assert_eq!(parsed.gate, s.gate);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_settings_text("hello world\nfoo = bar\n").is_err());
        assert!(parse_settings_text("").is_err());
        assert!(parse_settings_text("# gate = on\n").is_err());
        assert!(parse_settings_text("gate = off\n").is_ok());
    }

    #[test]
    fn age_text() {
        assert_eq!(ago(10), "just now");
        assert_eq!(ago(300), "5 min ago");
        assert_eq!(ago(7200), "2 h ago");
        assert_eq!(ago(86400), "1 day ago");
        assert_eq!(ago(3 * 86400), "3 days ago");
    }
}
