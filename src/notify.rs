//! Windows toast notifications for an unpackaged app.
//!
//! Toasts need an AppUserModelId registered under HKCU; we write one named
//! `Exel.Hush` with a display name and a PNG icon. Failures are ignored on
//! purpose: the in-app banner always shows the same information.

use std::path::PathBuf;
use windows::{
    core::HSTRING,
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
};
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

const AUMID: &str = "Exel.Hush";

fn icon_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Hush")
        .join("toast-icon.png")
}

/// Registers the toast identity (name and icon). Safe to call on every start.
pub fn register() {
    let icon = icon_path();
    if let Some(dir) = icon.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let png = encode_png(64, 64, &crate::icon_art::render_icon(64));
    let _ = std::fs::write(&icon, png);
    if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(format!(r"Software\Classes\AppUserModelId\{AUMID}"))
    {
        let _ = key.set_value("DisplayName", &String::from("Hush"));
        let _ = key.set_value("IconUri", &icon.display().to_string());
    }
}

/// Shows a toast with a title and one line of text, without blocking.
pub fn show(title: &str, text: &str) {
    let xml = toast_xml(title, text);
    let _ = std::thread::Builder::new()
        .name("hush-toast".into())
        .spawn(move || {
            let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            let _ = show_blocking(&xml);
            if com {
                unsafe { CoUninitialize() };
            }
        });
}

fn show_blocking(xml: &str) -> windows::core::Result<()> {
    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn toast_xml(title: &str, text: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape(title),
        escape(text)
    )
}

// -- Minimal PNG encoder (stored deflate blocks, no extra crates) ----------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1_u32, 0_u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encodes 8-bit RGBA pixels as a PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in rgba.chunks(width as usize * 4) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(block) = blocks.next() {
        zlib.push(u8::from(blocks.peek().is_none()));
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_is_escaped() {
        let xml = toast_xml("A & B", "<Mic> \"x\"");
        assert!(xml.contains("A &amp; B"));
        assert!(xml.contains("&lt;Mic&gt; &quot;x&quot;"));
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn png_has_signature_and_chunks() {
        let png = encode_png(2, 2, &[255; 16]);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert!(png.windows(4).any(|w| w == b"IDAT"));
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }
}

#[cfg(test)]
mod icon_preview {
    /// Writes icon previews as PNGs: `HUSH_ICON_PREVIEW=<dir> cargo test
    /// --bin hush icon_preview -- --ignored`
    #[test]
    #[ignore]
    fn write_previews() {
        let dir = std::path::PathBuf::from(std::env::var("HUSH_ICON_PREVIEW").unwrap());
        for size in [16, 24, 32, 48, 256] {
            for (active, tag) in [(true, "on"), (false, "off")] {
                let rgba = crate::icon_art::render_icon_state(size, active);
                let png = super::encode_png(size, size, &rgba);
                std::fs::write(dir.join(format!("icon-{size}-{tag}.png")), png).unwrap();
            }
        }
    }
}
