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
/// `DeviceState` value of a plugged-in endpoint.
const STATE_ACTIVE: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The GUID part of the endpoint id (registry key name).
    pub id: String,
    /// Friendly name, e.g. "Microphone (USB Audio)"; the id if unknown.
    pub name: String,
    pub container: Option<String>,
    pub effect_conflict: bool,
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
    hush::device_registry::label(&props)
}

/// Friendly names compare ignoring case and surrounding spaces.
pub fn names_match(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Full MMDevice id of a capture endpoint: "{0.0.1.00000000}.{guid}".
pub fn full_device_id(id: &str) -> String {
    format!("{{0.0.1.00000000}}.{id}")
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
            let endpoint = root.open_subkey_with_flags(&id, KEY_READ).ok();
            use hush::device_registry::Effect;
            let effect = endpoint
                .as_ref()
                .filter(|_| active)
                .map(hush::device_registry::effect);
            let has_hush = effect == Some(Effect::Ready);
            let effect_conflict = matches!(effect, Some(Effect::Conflict | Effect::Foreign));
            let container = endpoint
                .as_ref()
                .and_then(|key| key.open_subkey_with_flags("Properties", KEY_READ).ok())
                .as_ref()
                .and_then(hush::device_registry::container);
            Endpoint {
                has_hush,
                effect_conflict,
                container,
                enhancements_off: active
                    && endpoint
                        .as_ref()
                        .is_some_and(hush::device_registry::enhancements_disabled),
                id,
                name,
                active,
            }
        })
        .collect()
}

/// Plugged-in physical capture endpoints.
pub fn physical_microphones() -> Vec<Endpoint> {
    use hush::virtual_output::{
        is_hush_virtual_input, is_hush_virtual_output, is_third_party_virtual_endpoint,
    };
    capture_endpoints()
        .into_iter()
        .filter(|e| {
            e.active
                && !is_hush_virtual_input(&e.name)
                && !is_hush_virtual_output(&e.name)
                && !is_third_party_virtual_endpoint(&e.name)
        })
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
