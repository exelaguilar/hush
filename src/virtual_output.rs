//! Recognising virtual audio devices by name (Hush's own and other
//! products'), so only physical microphones are offered.

// Windows may append the adapter name in parentheses, so matching is by substring.

/// The driver's render endpoint, which Hush writes processed mic audio to.
pub fn is_hush_virtual_output(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("hush") && lower.contains("virtual microphone input")
}

/// The driver's capture endpoint ("Hush Microphone") that Discord, games
/// etc. record from. Never the render endpoint, even though its name also
/// contains "microphone".
pub fn is_hush_virtual_input(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("hush") && lower.contains("microphone") && !is_hush_virtual_output(name)
}

/// Another product's virtual endpoint (SteelSeries Sonar, VB-CABLE, ...).
/// Hush does not depend on these; using one as the microphone source
/// would route the voice through that product's processing first.
pub fn is_third_party_virtual_endpoint(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "sonar",
        "vb-audio",
        "cable output",
        "cable input",
        "voicemeeter",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_hush_endpoints() {
        let render = "Hush Virtual Microphone Input (Hush Virtual Audio Device (WDM))";
        let capture = "Hush Microphone (Hush Virtual Audio Device (WDM))";
        assert!(is_hush_virtual_output(render));
        assert!(!is_hush_virtual_input(render));
        assert!(is_hush_virtual_input(capture));
        assert!(!is_hush_virtual_output(capture));
        for other in [
            "Microphone (fifine SC3)",
            "Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)",
            "CABLE Output (VB-Audio Virtual Cable)",
        ] {
            assert!(!is_hush_virtual_input(other));
            assert!(!is_hush_virtual_output(other));
        }
        assert!(is_third_party_virtual_endpoint(
            "SteelSeries Sonar - Microphone (SteelSeries Sonar Virtual Audio Device)"
        ));
        assert!(is_third_party_virtual_endpoint(
            "CABLE Output (VB-Audio Virtual Cable)"
        ));
        assert!(!is_third_party_virtual_endpoint("Microphone (fifine SC3)"));
        assert!(!is_third_party_virtual_endpoint(render));
    }
}
