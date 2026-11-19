//! Capture endpoints as Windows records them in the registry (read only, no
//! admin needed): friendly name, whether the device is plugged in, whether
//! Hush's plugin is attached, and whether "Audio enhancements" are off.
//!
//! This is the one place that maps friendly names to endpoint ids.

use winreg::{
    enums::{HKEY_LOCAL_MACHINE, KEY_READ},
    RegKey,
};

const MMDEVICES: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio";
const PKEY_NAME: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
const PKEY_DESCRIPTION: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";
/// 1 = "Audio enhancements" disabled: Windows then skips every APO.
const PKEY_DISABLE_SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";
const FX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}";
/// `DeviceState` value of a plugged-in endpoint.
const STATE_ACTIVE: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The GUID part of the endpoint id (registry key name).
    pub id: String,
    /// Friendly name, e.g. "Microphone (USB Audio)"; the id if unknown.
    pub name: String,
    pub active: bool,
    /// Hush's plugin is registered on this endpoint. Only checked for
    /// active endpoints.
    pub has_hush: bool,
    /// "Audio enhancements" are switched off. Only checked for active
    /// endpoints.
    pub enhancements_off: bool,
}

/// Friendly name of an endpoint of any flow ("Capture" or "Render").
pub fn endpoint_name(flow: &str, id: &str) -> Option<String> {
    let props = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"{MMDEVICES}\{flow}\{id}\Properties"), KEY_READ)
        .ok()?;
    let short: String = props.get_value(PKEY_NAME).ok()?;
    let description: String = props.get_value(PKEY_DESCRIPTION).unwrap_or_default();
    Some(if description.is_empty() {
        short
    } else {
        format!("{short} ({description})")
    })
}

/// Friendly names compare ignoring case and surrounding spaces.
pub fn names_match(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Full MMDevice id of a capture endpoint: "{0.0.1.00000000}.{guid}".
pub fn full_device_id(id: &str) -> String {
    format!("{{0.0.1.00000000}}.{id}")
}

fn hush_attached(root: &RegKey, id: &str) -> bool {
    let Ok(key) = root.open_subkey_with_flags(format!(r"{id}\FxProperties"), KEY_READ) else {
        return false;
    };
    [1, 2, 7].into_iter().any(|slot| {
        key.get_value::<String, _>(format!("{FX},{slot}"))
            .is_ok_and(|v| v.eq_ignore_ascii_case(hush_apo::CLSID_STRING))
    })
}

/// The switch lives in Properties on most builds and in FxProperties on some.
fn enhancements_disabled(root: &RegKey, id: &str) -> bool {
    ["Properties", "FxProperties"].into_iter().any(|sub| {
        root.open_subkey_with_flags(format!(r"{id}\{sub}"), KEY_READ)
            .and_then(|key| key.get_value::<u32, _>(PKEY_DISABLE_SYSFX))
            .is_ok_and(|v| v == 1)
    })
}

/// Every capture endpoint Windows knows, plugged in or not.
pub fn capture_endpoints() -> Vec<Endpoint> {
    let Ok(root) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"{MMDEVICES}\Capture"), KEY_READ)
    else {
        return Vec::new();
    };
    root.enum_keys()
        .flatten()
        .map(|id| {
            let active = root
                .open_subkey_with_flags(&id, KEY_READ)
                .and_then(|key| key.get_value::<u32, _>("DeviceState"))
                .is_ok_and(|state| state == STATE_ACTIVE);
            let name = endpoint_name("Capture", &id).unwrap_or_else(|| id.clone());
            Endpoint {
                has_hush: active && hush_attached(&root, &id),
                enhancements_off: active && enhancements_disabled(&root, &id),
                id,
                name,
                active,
            }
        })
        .collect()
}

/// Plugged-in capture endpoints whose friendly name is `name`.
pub fn find_active(name: &str) -> Vec<Endpoint> {
    capture_endpoints()
        .into_iter()
        .filter(|endpoint| endpoint.active && names_match(&endpoint.name, name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_ignore_case_and_padding() {
        assert!(names_match(" Mic (USB) ", "mic (usb)"));
        assert!(!names_match("Mic (USB)", "Mic (USB 2)"));
    }

    #[test]
    fn full_id_has_the_capture_prefix() {
        assert_eq!(full_device_id("{abc}"), "{0.0.1.00000000}.{abc}");
    }
}
