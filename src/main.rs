//! Hush: system-tray app with live controls for the clean-mic APO and
//! a built-in microphone test.
//!
//! One executable, two roles: `hush.exe` is the tray agent (always
//! running, no graphics) and `hush.exe --window` is the window, started by
//! the agent while it is open. See `ipc`.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod agent;
#[cfg(windows)]
mod app_names;
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
mod ipc;
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
    let args: Vec<String> = std::env::args().collect();
    let page = args
        .iter()
        .position(|arg| arg == ipc::PAGE_FLAG)
        .and_then(|i| args.get(i + 1))
        .cloned();
    // Developer aid (debug builds): HUSH_SHOT runs a window beside an
    // installed Hush.
    if dev::shot_path().is_some() {
        return ui::run(page);
    }
    if args.iter().any(|arg| arg == ipc::WINDOW_FLAG) {
        // If a window is already open, it comes forward instead.
        let Some(_window) = instance::acquire(r"Local\Hush.Window", ipc::EV_WINDOW_SHOW) else {
            return Ok(());
        };
        return ui::run(page);
    }
    // Second launch (Start menu, shortcut, or a clicked toast, which runs
    // `hush.exe --show`): the running agent shows the window, and this one
    // exits. When none is running this one becomes the agent.
    let Some(_agent) = instance::acquire(r"Local\Hush.App", ipc::EV_SHOW) else {
        return Ok(());
    };
    let show = args
        .iter()
        .any(|arg| arg == notify::SHOW_FLAG || arg.starts_with("hush:"));
    let start_hidden = !show && args.iter().any(|arg| arg == prefs::MINIMIZED_FLAG);
    agent::run(!start_hidden, page);
    Ok(())
}
