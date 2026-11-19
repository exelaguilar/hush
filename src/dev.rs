//! Developer aids driven by environment variables. They exist only in debug
//! builds: a release build never reads them.
//!
//! * `HUSH_SHOT=<file.png>` saves a screenshot and exits (and runs beside an
//!   installed Hush). `HUSH_SHOT_DELAY` (ms), `HUSH_SHOT_SCALE`,
//!   `HUSH_SHOT_SIZE=820x760` and `HUSH_SHOT_TEST=1` (open the mic so the
//!   meters move) tune it.
//! * `HUSH_PAGE=microphone|noise|voice|test|settings|about` opens a page.
//! * `HUSH_THEME=light|dark` overrides the Windows theme.
//! * `HUSH_DEMO_TOAST=1` shows the "recording saved" toast.

use eframe::egui;
use std::path::PathBuf;

#[cfg(debug_assertions)]
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(not(debug_assertions))]
fn var(_name: &str) -> Option<String> {
    None
}

pub fn shot_path() -> Option<PathBuf> {
    var("HUSH_SHOT").map(PathBuf::from)
}

pub fn shot_delay_ms() -> u64 {
    var("HUSH_SHOT_DELAY")
        .and_then(|ms| ms.parse().ok())
        .unwrap_or(1500)
}

pub fn shot_scale() -> Option<f32> {
    var("HUSH_SHOT_SCALE").and_then(|s| s.parse().ok())
}

pub fn shot_test() -> bool {
    var("HUSH_SHOT_TEST").is_some()
}

pub fn demo_toast() -> bool {
    var("HUSH_DEMO_TOAST").is_some()
}

/// Page name from `HUSH_PAGE`, lower case.
pub fn page() -> Option<String> {
    var("HUSH_PAGE")
}

pub fn theme() -> Option<egui::ThemePreference> {
    match var("HUSH_THEME")?.as_str() {
        "light" => Some(egui::ThemePreference::Light),
        "dark" => Some(egui::ThemePreference::Dark),
        _ => None,
    }
}

/// Window size: 820x600 by default, or `HUSH_SHOT_SIZE` times `HUSH_SHOT_SCALE`.
pub fn window_size() -> [f32; 2] {
    let scale = shot_scale().unwrap_or(1.0);
    let [w, h] = var("HUSH_SHOT_SIZE")
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some([w.parse().ok()?, h.parse().ok()?])
        })
        .unwrap_or([820.0, 600.0]);
    [w * scale, h * scale]
}
