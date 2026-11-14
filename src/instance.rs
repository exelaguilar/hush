//! One Hush per user session. A second launch (Start menu, shortcut)
//! signals the running instance to show its window, then exits.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::{
    Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM},
    System::Threading::{
        CreateEventW, CreateMutexW, GetCurrentProcessId, OpenEventW, SetEvent, WaitForSingleObject,
        EVENT_MODIFY_STATE, INFINITE,
    },
    UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        SetForegroundWindow, ShowWindow, ASFW_ANY, SW_RESTORE, SW_SHOW,
    },
};
use windows_core::{w, BOOL};

/// Shows and focuses this process's main window ("Hush") directly via
/// Win32. Works while the UI loop is idle with the window hidden, which is
/// exactly when egui's own viewport commands are not processed promptly.
pub fn show_own_window() {
    unsafe extern "system" fn visit(hwnd: HWND, _: LPARAM) -> BOOL {
        let mut pid = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != unsafe { GetCurrentProcessId() } {
            return true.into();
        }
        let mut title = [0_u16; 64];
        let len = unsafe { GetWindowTextW(hwnd, &mut title) } as usize;
        if String::from_utf16_lossy(&title[..len]) != "Hush" {
            return true.into();
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
        false.into()
    }
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(0));
    }
}

/// Set when another launch asked us to show the window.
pub static SHOW_REQUESTED: AtomicBool = AtomicBool::new(false);

pub struct Instance {
    _mutex: HANDLE,
}

/// Returns `None` if Hush is already running (after asking it to
/// show itself).
pub fn acquire() -> Option<Instance> {
    unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\Hush.App")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Let the running instance bring its window to the foreground.
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\Hush.Show")) {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            }
            let _ = CloseHandle(mutex);
            return None;
        }
        Some(Instance { _mutex: mutex })
    }
}

/// Starts a thread that waits for "show" requests from later launches and
/// calls `wake` (which should request a repaint of the UI).
pub fn listen(wake: impl Fn() + Send + 'static) {
    let event = unsafe { CreateEventW(None, false, false, w!("Local\\Hush.Show")) };
    let Ok(event) = event else {
        return;
    };
    let event = event.0 as usize;
    std::thread::Builder::new()
        .name("hush-instance".into())
        .spawn(move || loop {
            let handle = HANDLE(event as *mut _);
            unsafe { WaitForSingleObject(handle, INFINITE) };
            show_own_window();
            SHOW_REQUESTED.store(true, Ordering::Release);
            wake();
        })
        .ok();
}
