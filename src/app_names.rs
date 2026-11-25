//! Friendly names for the apps using a microphone ("Apex Legends", not
//! "r5apex_dx12"): a short name for well-known apps, else the program's own
//! description, else the title of its window, else its file name.

use std::path::{Path, PathBuf};
use windows::{
    core::{HSTRING, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM},
        Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW},
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
            GW_OWNER,
        },
    },
};
use windows_core::BOOL;

/// Longest name shown; window titles can be long.
const MAX_CHARS: usize = 32;

/// Display name of a process, or `None` for Hush itself (its Mic Test is
/// not "an app using the microphone").
pub fn label(pid: u32) -> Option<String> {
    if pid == 0 {
        return Some("An app".into());
    }
    let Some(path) = image_path(pid) else {
        // Protected processes hide their path; their window still has a title.
        return pick("", None, window_title(pid).as_deref());
    };
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.eq_ignore_ascii_case("hush") || known(&stem).is_some() {
        return pick(&stem, None, None);
    }
    let description = file_description(&path);
    let title = description.is_none().then(|| window_title(pid)).flatten();
    pick(&stem, description.as_deref(), title.as_deref())
}

/// Short names for apps whose description is long or generic.
fn known(stem: &str) -> Option<&'static str> {
    Some(match stem.to_ascii_lowercase().as_str() {
        "obs64" | "obs32" | "obs" => "OBS",
        "msedge" | "msedgewebview2" => "Edge",
        "ms-teams" | "teams" => "Teams",
        "steamwebhelper" | "steam" => "Steam",
        "zoom" => "Zoom",
        "firefox" => "Firefox",
        "chrome" => "Chrome",
        "discord" | "discordptb" | "discordcanary" => "Discord",
        _ => return None,
    })
}

fn shorten(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_CHARS {
        return Some(text.to_string());
    }
    let cut: String = text.chars().take(MAX_CHARS - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

/// The name to show, from what is known about the process.
pub fn pick(stem: &str, description: Option<&str>, title: Option<&str>) -> Option<String> {
    if stem.eq_ignore_ascii_case("hush") {
        return None;
    }
    if let Some(name) = known(stem) {
        return Some(name.to_string());
    }
    if let Some(name) = description.and_then(shorten) {
        return Some(name);
    }
    if let Some(name) = title.and_then(shorten) {
        return Some(name);
    }
    let mut chars = stem.chars();
    Some(match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "An app".to_string(),
    })
}

fn image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0_u16; 1024];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        ok.then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize])))
    }
}

/// The program's "File description" from its version resource.
fn file_description(path: &Path) -> Option<String> {
    let file = HSTRING::from(path.as_os_str());
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(file.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0_u8; size as usize];
        GetFileVersionInfoW(PCWSTR(file.as_ptr()), None, size, data.as_mut_ptr().cast()).ok()?;
        // First language/code page pair, then that block's FileDescription.
        let mut pointer = std::ptr::null_mut();
        let mut len = 0_u32;
        let translation = HSTRING::from("\\VarFileInfo\\Translation");
        let (language, codepage) = if VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(translation.as_ptr()),
            &mut pointer,
            &mut len,
        )
        .as_bool()
            && len >= 4
        {
            let pair = pointer as *const u16;
            (*pair, *pair.add(1))
        } else {
            (0x0409, 0x04B0)
        };
        let key = HSTRING::from(format!(
            "\\StringFileInfo\\{language:04x}{codepage:04x}\\FileDescription"
        ));
        if !VerQueryValueW(data.as_ptr().cast(), PCWSTR(key.as_ptr()), &mut pointer, &mut len)
            .as_bool()
            || len == 0
        {
            return None;
        }
        let text = std::slice::from_raw_parts(pointer as *const u16, len as usize);
        let text = String::from_utf16_lossy(text);
        Some(text.trim_end_matches('\0').trim().to_string()).filter(|t| !t.is_empty())
    }
}

/// Title of the process's first visible top-level window.
fn window_title(pid: u32) -> Option<String> {
    struct Search {
        pid: u32,
        title: Option<String>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        let search = unsafe { &mut *(param.0 as *mut Search) };
        let mut owner = 0_u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        let top_level = unsafe { GetWindow(hwnd, GW_OWNER) }.is_err();
        if owner != search.pid || !top_level || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return true.into();
        }
        let mut buffer = [0_u16; 256];
        let len = unsafe { GetWindowTextW(hwnd, &mut buffer) } as usize;
        let title = String::from_utf16_lossy(&buffer[..len]);
        if title.trim().is_empty() {
            return true.into();
        }
        search.title = Some(title);
        false.into()
    }
    let mut search = Search { pid, title: None };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
    }
    search.title
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_prefer_known_then_description_then_title() {
        assert_eq!(pick("obs64", Some("OBS Studio"), None).as_deref(), Some("OBS"));
        assert_eq!(pick("Discord", None, None).as_deref(), Some("Discord"));
        assert_eq!(
            pick("r5apex_dx12", None, Some("Apex Legends")).as_deref(),
            Some("Apex Legends")
        );
        assert_eq!(
            pick("vrchat", Some("VRChat"), Some("VRChat - Home")).as_deref(),
            Some("VRChat")
        );
        assert_eq!(pick("vrchat", None, None).as_deref(), Some("Vrchat"));
        assert_eq!(pick("", None, None).as_deref(), Some("An app"));
    }

    #[test]
    fn hush_itself_is_not_listed() {
        assert_eq!(pick("hush", Some("Hush"), Some("Hush")), None);
        assert_eq!(pick("Hush", None, None), None);
    }

    #[test]
    fn long_titles_are_shortened() {
        let name = pick("game", None, Some(&"x".repeat(80))).unwrap();
        assert_eq!(name.chars().count(), MAX_CHARS);
        assert!(name.ends_with('…'));
    }

    #[test]
    fn this_test_process_has_a_name() {
        assert!(label(std::process::id()).is_some());
    }
}
