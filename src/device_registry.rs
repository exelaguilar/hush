//! Windows capture endpoint metadata shared by the app, installer and Setup.

use winreg::{enums::KEY_READ, RegKey};

const PKEY_NAME: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
const PKEY_DESCRIPTION: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";
const PKEY_CONTAINER: &str = "{8c7ed206-3f8a-4827-b3ab-ae9e1faefc6c},2";
pub const FX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}";
pub const FX_MODES: &str = "{d3993a3f-99c2-4402-b5ec-a92a0367664b}";
pub const MODES: [&str; 1] = ["{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}"];
pub const HUSH_CLSID: &str = "{8AA11902-CC87-49EB-B601-AA1449A395A6}";
pub const PKEY_DISABLE_SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    Missing,
    MissingModes,
    Foreign,
    Conflict,
    Ready,
}

pub fn effect(endpoint: &RegKey) -> Effect {
    use winreg::types::ToRegValue;
    let Ok(key) = endpoint.open_subkey_with_flags("FxProperties", KEY_READ) else {
        return Effect::Missing;
    };
    let installed = |slot| {
        key.get_value::<String, _>(format!("{FX},{slot}"))
            .is_ok_and(|value| value.eq_ignore_ascii_case(HUSH_CLSID))
    };
    if installed(1) || installed(2) {
        return Effect::Ready;
    }
    if !installed(7) {
        return if key.get_raw_value(format!("{FX},7")).is_ok()
            || key.get_raw_value(format!("{FX_MODES},7")).is_ok()
        {
            Effect::Foreign
        } else {
            Effect::Missing
        };
    }
    let expected: Vec<_> = MODES.iter().map(|mode| mode.to_string()).collect();
    match key.get_raw_value(format!("{FX_MODES},7")) {
        Ok(value) if value == expected.to_reg_value() => Effect::Ready,
        Ok(_) => Effect::Conflict,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Effect::MissingModes,
        Err(_) => Effect::Conflict,
    }
}

pub fn label(properties: &RegKey) -> Option<String> {
    let short: String = properties.get_value(PKEY_NAME).ok()?;
    if short.is_empty() {
        return None;
    }
    let description: String = properties.get_value(PKEY_DESCRIPTION).unwrap_or_default();
    Some(if description.is_empty() {
        short
    } else {
        format!("{short} ({description})")
    })
}

#[allow(dead_code)]
pub fn container(properties: &RegKey) -> Option<String> {
    // A container groups the endpoints of one physical device. Leave
    // unidentified endpoints or the system-wide container unassigned.
    if let Ok(value) = properties.get_value::<String, _>(PKEY_CONTAINER) {
        let value: String = value
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect::<String>()
            .to_ascii_lowercase();
        if value.len() == 32
            && !matches!(
                value.as_str(),
                "00000000000000000000000000000000" | "0000000000000000ffffffffffffffff"
            )
        {
            return Some(value);
        }
    }
    let raw = properties.get_raw_value(PKEY_CONTAINER).ok()?;
    let mut bytes: [u8; 16] = raw.bytes.try_into().ok()?;
    if bytes == [0; 16]
        || bytes
            == [
                0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255,
            ]
    {
        return None;
    }
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    Some(
        bytes
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

#[allow(dead_code)]
pub fn enhancements_disabled(endpoint: &RegKey) -> bool {
    ["Properties", "FxProperties"].into_iter().any(|sub| {
        endpoint
            .open_subkey_with_flags(sub, KEY_READ)
            .and_then(|key| key.get_value::<u32, _>(PKEY_DISABLE_SYSFX))
            .is_ok_and(|v| v == 1)
    })
}
