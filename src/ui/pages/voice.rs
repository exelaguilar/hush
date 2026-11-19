//! Voice page: presets, equalizer and level controls.

use super::super::HushApp;
use crate::theme::{self, palette};
use eframe::egui::{self, RichText};
use hush_apo::settings::Preset;

impl HushApp {
    pub(in crate::ui) fn voice_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Voice",
            "Shape the tone of your voice and keep its volume even.",
        );
        let mut changed = false;
        ui.add_enabled_ui(self.settings.enabled, |ui| {
            theme::card(
                ui,
                Some("Preset"),
                Some("A starting point; adjust the sliders to taste."),
                |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let p = palette(ui);
                        for preset in Preset::ALL {
                            let selected = self.settings.preset == preset;
                            let button = egui::Button::new(if selected {
                                theme::semibold_text(preset.label()).color(p.primary_foreground)
                            } else {
                                RichText::new(preset.label())
                            })
                            .fill(if selected { p.primary } else { p.background })
                            .stroke(egui::Stroke::new(
                                1.0,
                                if selected { p.primary } else { p.border },
                            ))
                            .corner_radius(6);
                            if ui.add(button).clicked() && !selected {
                                preset.apply(&mut self.settings);
                                changed = true;
                            }
                        }
                    });
                },
            );
            ui.add_space(12.0);
            theme::card(ui, Some("Equalizer"), None, |ui| {
                let mut eq_changed = false;
                theme::row_sized(
                    ui,
                    "Bass",
                    "Body and fullness.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        eq_changed |= ui
                            .add(
                                egui::Slider::new(&mut self.settings.bass_db, -6.0..=9.0)
                                    .suffix(" dB")
                                    .step_by(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Clarity",
                    "Presence; helps you cut through.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        eq_changed |= ui
                            .add(
                                egui::Slider::new(&mut self.settings.clarity_db, -6.0..=9.0)
                                    .suffix(" dB")
                                    .step_by(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Air",
                    "Crispness. Too much can sound hissy.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        eq_changed |= ui
                            .add(
                                egui::Slider::new(&mut self.settings.air_db, -6.0..=9.0)
                                    .suffix(" dB")
                                    .step_by(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Saturation",
                    "Subtle tape-style warmth, like a good preamp. 0 is off.",
                    theme::SLIDER_WIDTH,
                    |ui| {
                        let mut warmth = self.settings.warmth * 100.0;
                        if ui
                            .add(
                                egui::Slider::new(&mut warmth, 0.0..=100.0)
                                    .suffix(" %")
                                    .integer(),
                            )
                            .changed()
                        {
                            self.settings.warmth = warmth / 100.0;
                            changed = true;
                        }
                    },
                );
                if eq_changed {
                    self.settings.preset = Preset::Custom;
                    changed = true;
                }
            });
            ui.add_space(12.0);
            theme::card(ui, Some("Level"), None, |ui| {
                theme::row(
                    ui,
                    "Auto level",
                    "Keeps your voice at a steady volume, like a broadcast engineer would.",
                    |ui| {
                        changed |=
                            theme::switch(ui, &mut self.settings.leveler, "Auto level").changed();
                    },
                );
                theme::separator(ui);
                theme::row(
                    ui,
                    "De-esser",
                    "Softens harsh \"s\" and \"t\" sounds.",
                    |ui| {
                        changed |=
                            theme::switch(ui, &mut self.settings.deesser, "De-esser").changed();
                    },
                );
                theme::separator(ui);
                theme::row(
                    ui,
                    "Rumble & pop filter",
                    "Removes desk bumps, low hum and \"p\"/\"b\" pops.",
                    |ui| {
                        changed |=
                            theme::switch(ui, &mut self.settings.high_pass, "Rumble & pop filter")
                                .changed();
                    },
                );
                theme::separator(ui);
                theme::row_sized(ui, "Output volume", "", theme::SLIDER_WIDTH, |ui| {
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut self.settings.gain_db, -12.0..=12.0)
                                .suffix(" dB")
                                .step_by(0.5)
                                .fixed_decimals(1),
                        )
                        .changed();
                });
            });
        });
        if changed {
            self.mark_dirty();
        }
    }
}
