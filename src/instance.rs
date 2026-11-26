//! One tray agent and one window per user session. A second launch
//! (Start menu, shortcut, a clicked toast) signals the running one, then
//! exits.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::{
    Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM},
    System::Threading::{CreateMutexW, GetCurrentProcessId},
    UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        SetForegroundWindow, ShowWindow, ASFW_ANY, SW_RESTORE, SW_SHOW,
    },
};
use windows_core::BOOL;

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

/// Takes the named mutex. Returns `None` if another process holds it,
/// after setting that process's `show_event`.
pub fn acquire(mutex_name: &str, show_event: &str) -> Option<Instance> {
    let name = windows_core::HSTRING::from(mutex_name);
    unsafe {
        let mutex = CreateMutexW(None, true, windows_core::PCWSTR(name.as_ptr())).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Let the running instance bring its window to the foreground.
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !crate::ipc::signal(show_event) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let _ = CloseHandle(mutex);
            return None;
        }
        Some(Instance { _mutex: mutex })
    }
}

/// Window process: waits for `event` ("show the window") on a thread of
/// its own, brings the window forward and calls `wake`.
pub fn listen(event: &str, wake: impl Fn() + Send + 'static) {
    crate::ipc::on_event(event, move || {
        show_own_window();
        SHOW_REQUESTED.store(true, Ordering::Release);
        wake();
    });
}
