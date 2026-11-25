//! Mic Test page: opens the microphone like a call app and lets you listen.

use super::super::{open_in_shell, HushApp};
use crate::{
    mic_test,
    theme::{self, palette},
};
use eframe::egui::{self, RichText};
use std::time::Instant;

impl HushApp {
    pub(in crate::ui) fn test_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Mic Test",
            "Hear exactly what others hear. Toggle settings while listening to compare.",
        );
        let p = palette(ui);
        if self.test_state.mic.is_none() {
            theme::card(ui, None, None, |ui| {
                theme::row(
                    ui,
                    "Microphone test",
                    "Opens the microphone the same way Discord does.",
                    |ui| {
                        if theme::primary_button(ui, "Start test").clicked() {
                            self.start_test();
                        }
                    },
                );
                theme::separator(ui);
                theme::row_sized(
                    ui,
                    "Saved recordings",
                    "Takes you saved with Save are kept in this folder.",
                    170.0,
                    |ui| {
                        if theme::outline_button(ui, "Open recordings folder").clicked() {
                            self.open_recordings_folder();
                        }
                    },
                );
                if let Some(error) = &self.test_state.error {
                    ui.label(RichText::new(error).small().color(p.destructive));
                }
            });
            return;
        }

        let level_db = 20.0 * self.test_state.meter.max(1e-5).log10();
        let fill = ((level_db + 60.0) / 60.0).clamp(0.0, 1.0);
        theme::card(ui, Some("What others hear"), None, |ui| {
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 5.0, p.track);
            let mut bar = rect;
            bar.set_width(rect.width() * fill);
            let color = if level_db > -3.0 {
                p.destructive
            } else if level_db > -12.0 {
                p.warning
            } else {
                p.primary
            };
            ui.painter().rect_filled(bar, 5.0, color);
            if level_db < -60.0 {
                theme::hint(ui, "Silent");
            } else {
                theme::hint(ui, &format!("{level_db:.0} dB"));
            }
            theme::hint(
                ui,
                "Level after Clean Mic. Silence between words is normal.",
            );
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Compare"), None, |ui| {
            theme::row(
                ui,
                "Original vs. Clean Mic",
                "With Hear myself on, hold the button to hear your raw microphone.",
                |ui| self.compare_button(ui),
            );
        });
        ui.add_space(12.0);
        let mut stop = false;
        let mut saved = None;
        if let Some(test) = &self.test_state.mic {
            theme::card(ui, Some("Listen"), None, |ui| {
                theme::row(
                    ui,
                    "Hear myself",
                    &format!("Plays through {}. Use headphones.", test.output_name),
                    |ui| {
                        if theme::switch(ui, &mut self.test_state.monitor, "Hear myself").changed()
                        {
                            if let Err(error) = test.set_monitor(self.test_state.monitor) {
                                self.test_state.monitor = false;
                                self.test_state.error = Some(format!("{error:#}"));
                            }
                        }
                    },
                );
                theme::separator(ui);
                theme::row(
                    ui,
                    "Record and play back",
                    &format!(
                        "Records {} seconds. Try once with Clean Mic on and once off.",
                        mic_test::RECORD_SECONDS
                    ),
                    |ui| match test.recording_progress() {
                        Some(progress) => {
                            if theme::outline_button(ui, "Stop").clicked() {
                                test.stop_recording();
                            }
                            ui.add(egui::ProgressBar::new(progress).desired_width(120.0));
                        }
                        None => {
                            if test.is_playing() {
                                if theme::outline_button(ui, "Stop playback").clicked() {
                                    test.stop_playback();
                                }
                            } else if ui
                                .add_enabled(
                                    test.has_recording(),
                                    egui::Button::new("Play").corner_radius(6),
                                )
                                .clicked()
                            {
                                if let Err(error) = test.play_recording() {
                                    self.test_state.error = Some(format!("{error:#}"));
                                }
                            }
                            if test.has_recording() && theme::outline_button(ui, "Save").clicked() {
                                saved = Some(test.save_recording().map_err(|e| format!("{e:#}")));
                            }
                            if theme::outline_button(ui, "Record").clicked() {
                                test.start_recording();
                            }
                        }
                    },
                );
            });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if theme::outline_button(ui, "Stop test").clicked() {
                    stop = true;
                }
                if theme::outline_button(ui, "Open recordings folder").clicked() {
                    self.open_recordings_folder();
                }
            });
        }
        if stop {
            self.test_state.mic = None;
            self.test_state.monitor = false;
        }
        if let Some(result) = saved {
            self.test_state.saved = Some((result, Instant::now()));
        }
    }

    pub(in crate::ui) fn open_recordings_folder(&self) {
        let dir = mic_test::recordings_dir();
        // Explorer falls back to Documents if the folder does not exist yet.
        let _ = std::fs::create_dir_all(&dir);
        open_in_shell(&dir.display().to_string());
    }
}
