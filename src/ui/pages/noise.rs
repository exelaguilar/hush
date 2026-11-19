//! Noise & Gate page: live meters, noise cancellation and the voice gate.

use super::super::HushApp;
use crate::theme::{self, palette};
use eframe::egui::{self, RichText};

impl HushApp {
    /// Live meters from the plugin (works whenever any app uses the mic).
    pub(in crate::ui) fn live_card(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let live = self.live_levels.filter(|l| l.age_ms() < 1500);
        theme::card(
            ui,
            Some("Live"),
            Some("What another app (Discord, a game) is receiving right now. Hush itself never records you."),
            |ui| {
            match live {
                None => theme::hint(
                    ui,
                    "No app is using your microphone right now. Join a Discord call, or use Mic Test, to see the meters.",
                ),
                Some(levels) => {
                    let meter = |ui: &mut egui::Ui, label: &str, value: f32| {
                        ui.horizontal(|ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(120.0, 16.0),
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.set_min_width(120.0);
                                    ui.label(RichText::new(label).small());
                                },
                            );
                            let db = 20.0 * value.max(1e-5).log10();
                            let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width() - 4.0, 8.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(rect, 4.0, p.track);
                            let mut bar = rect;
                            bar.set_width(rect.width() * fill);
                            ui.painter().rect_filled(bar, 4.0, p.primary);
                        });
                    };
                    meter(ui, "Your mic (raw)", levels.input);
                    meter(ui, "What others hear", levels.output);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let (color, text) = if levels.voice {
                            (p.success, "Speaking: passing your voice")
                        } else {
                            (egui::Color32::GRAY, "Quiet: others hear silence")
                        };
                        theme::badge(ui, color, text);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            self.compare_button(ui);
                        });
                    });
                }
            }
        });
        ui.add_space(12.0);
    }

    pub(in crate::ui) fn noise_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Noise & Gate",
            "Remove background noise and silence the mic between words.",
        );
        self.live_card(ui);
        let mut changed = false;
        ui.add_enabled_ui(self.settings.enabled, |ui| {
            theme::card(ui, Some("Noise cancellation"), Some("AI noise removal (DeepFilterNet 3): keyboard, fans, clicks, room noise."), |ui| {
                theme::row(ui, "Enabled", "", |ui| {
                    changed |= theme::switch(ui, &mut self.settings.noise_suppression, "Noise cancellation").changed();
                });
                theme::separator(ui);
                ui.add_enabled_ui(self.settings.noise_suppression, |ui| {
                    theme::row_sized(ui, "Strength", "Lower sounds more natural; higher removes more.", theme::SLIDER_WIDTH, |ui| {
                        let mut strength = self.settings.suppression * 100.0;
                        if ui
                            .add(egui::Slider::new(&mut strength, 0.0..=100.0).suffix(" %").integer())
                            .changed()
                        {
                            self.settings.suppression = strength / 100.0;
                            changed = true;
                        }
                    });
                    theme::separator(ui);
                    theme::row(
                        ui,
                        "Click suppression",
                        "Softens hard keyboard and mouse clicks while you talk.",
                        |ui| {
                            changed |= theme::switch(ui, &mut self.settings.click_suppression, "Click suppression").changed();
                        },
                    );
                });
            });
            ui.add_space(12.0);
            ui.add_enabled_ui(self.settings.noise_suppression, |ui| {
                theme::card(ui, Some("Voice gate"), Some("Closes the mic when you are not talking. Short sounds like key clicks never open it."), |ui| {
                    theme::row(ui, "Enabled", "", |ui| {
                        changed |= theme::switch(ui, &mut self.settings.gate, "Voice gate").changed();
                    });
                    theme::separator(ui);
                    ui.add_enabled_ui(self.settings.gate, |ui| {
                        theme::row_sized(
                            ui,
                            "Sensitivity",
                            "Lower if noises open the mic; raise if soft words get cut.",
                            theme::SLIDER_WIDTH,
                            |ui| {
                                let mut sensitivity = self.settings.gate_sensitivity * 100.0;
                                if ui
                                    .add(egui::Slider::new(&mut sensitivity, 0.0..=100.0).suffix(" %").integer())
                                    .changed()
                                {
                                    self.settings.gate_sensitivity = sensitivity / 100.0;
                                    changed = true;
                                }
                            },
                        );
                    });
                });
            });
        });
        if changed {
            self.mark_dirty();
        }
    }
}
