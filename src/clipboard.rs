//! Copies text to the Windows clipboard.
//!
//! eframe is built without its clipboard feature (it would pull in a
//! clipboard crate), so `egui::Context::copy_text` does nothing here.

use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
    },
};

const CF_UNICODETEXT: u32 = 13;

/// Puts `text` on the clipboard. Returns false if Windows refused.
pub fn copy(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // Another app can hold the clipboard for a moment; retry briefly.
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !opened {
        return false;
    }
    let ok = unsafe { put(&wide) };
    unsafe {
        let _ = CloseClipboard();
    }
    ok
}

unsafe fn put(wide: &[u16]) -> bool {
    if EmptyClipboard().is_err() {
        return false;
    }
    let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(wide)) else {
        return false;
    };
    let target = GlobalLock(memory) as *mut u16;
    if target.is_null() {
        let _ = GlobalFree(Some(memory));
        return false;
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
    let _ = GlobalUnlock(memory);
    // On success the clipboard owns the memory.
    if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))).is_err() {
        let _ = GlobalFree(Some(HGLOBAL(memory.0)));
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    /// Touches the real clipboard, so it only runs on request:
    /// `cargo test --bin hush clipboard -- --ignored`
    #[test]
    #[ignore]
    fn copies_text() {
        assert!(super::copy("Hush clipboard test ✓"));
    }
}
