//! Hush: system-tray app with live controls for the clean-mic APO and
//! a built-in microphone test.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod dev;
#[cfg(windows)]
mod diagnostics;
#[cfg(windows)]
mod endpoints;
#[cfg(windows)]
mod health;
#[cfg(windows)]
mod icon_art;
#[cfg(windows)]
mod install_state;
#[cfg(windows)]
mod instance;
#[cfg(windows)]
mod level_watch;
#[cfg(windows)]
mod mic_test;
#[cfg(windows)]
mod mute;
#[cfg(windows)]
mod notify;
#[cfg(windows)]
mod prefs;
#[cfg(windows)]
mod settings_io;
#[cfg(windows)]
mod theme;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod updates;

#[cfg(not(windows))]
fn main() {
    println!("Hush is Windows-only.");
}

#[cfg(windows)]
fn uninstall() {
    use std::os::windows::process::CommandExt;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONQUESTION, MB_YESNO};
    let answer = unsafe {
        MessageBoxW(
            None,
            windows_core::w!(
                "Remove Hush and Clean Mic from this PC?\n\nWindows audio restarts for a moment."
            ),
            windows_core::w!("Uninstall Hush"),
            MB_YESNO | MB_ICONQUESTION,
        )
    };
    if answer != IDYES {
        return;
    }
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.to_path_buf()))
    {
        let _ = std::process::Command::new(dir.join("hush-setup.exe"))
            .arg("uninstall-app")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}

#[cfg(windows)]
fn main() -> eframe::Result {
    // Windows "Installed apps" > Uninstall runs `hush.exe --uninstall`:
    // confirm in a normal dialog, then remove everything without a console.
    if std::env::args().any(|arg| arg == "--uninstall") {
        uninstall();
        return Ok(());
    }
    // Second launch (Start menu, shortcut, or a clicked toast, which runs
    // `hush.exe --show`): bring the running Hush to the front and exit. When
    // none is running this one starts and shows its window.
    // Developer aid (debug builds): HUSH_SHOT runs beside an installed Hush.
    let _instance = if dev::shot_path().is_some() {
        None
    } else {
        let Some(instance) = instance::acquire() else {
            return Ok(());
        };
        Some(instance)
    };
    let show = std::env::args().any(|arg| arg == notify::SHOW_FLAG || arg.starts_with("hush:"));
    let start_hidden = !show && std::env::args().any(|arg| arg == prefs::MINIMIZED_FLAG);
    let icon = eframe::egui::IconData {
        rgba: icon_art::render_icon(64),
        width: 64,
        height: 64,
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Hush")
            .with_inner_size(dev::window_size())
            .with_min_inner_size([680.0, 480.0])
            .with_icon(icon)
            .with_visible(!start_hidden),
        // Open in the middle of the screen instead of the top-left corner.
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Hush",
        options,
        Box::new(move |cc| Ok(Box::new(ui::HushApp::new(&cc.egui_ctx, start_hidden)))),
    )
}
