//! Microphone page: choose the microphone and install Clean Mic on it.

use super::super::{open_in_shell, HushApp};
use crate::{
    level_watch,
    theme::{self, palette},
};
use eframe::egui::{self, RichText};

impl HushApp {
    pub(in crate::ui) fn microphone_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(
            ui,
            "Microphone",
            "Choose your microphone and install Clean Mic on it. Apps keep using that microphone as usual.",
        );
        self.health_banner(ui);
        let attached_here = self.attached_here();
        let mut refresh = false;
        theme::card(ui, Some("Device"), None, |ui| {
            theme::row_sized(
                ui,
                "Microphone",
                "The physical microphone to clean up.",
                theme::COMBO_WIDTH + 84.0,
                |ui| {
                    if theme::outline_button(ui, "Refresh")
                        .on_hover_text("Rescan microphones")
                        .clicked()
                    {
                        refresh = true;
                    }
                    let mut chosen = self.selected_mic.clone();
                    egui::ComboBox::from_id_salt("mic")
                        .selected_text(chosen.clone())
                        .show_ui(ui, |ui| {
                            for mic in &self.mics {
                                ui.selectable_value(&mut chosen, mic.clone(), mic);
                            }
                        });
                    if chosen != self.selected_mic {
                        self.selected_mic = chosen;
                        self.mute.set_device(&self.selected_mic);
                        if self.test_state.mic.take().is_some() {
                            self.start_test();
                        }
                    }
                },
            );
            theme::separator(ui);
            theme::row_sized(
                ui,
                "Mic position",
                "On the desk gives a distant mic more boost and protects softer word endings.",
                theme::MIC_POSITION_WIDTH,
                |ui| {
                    let mut position = Some(self.settings.desk_mic);
                    theme::mic_position(ui, &mut position, false);
                    if let Some(desk_mic) =
                        position.filter(|&desk_mic| desk_mic != self.settings.desk_mic)
                    {
                        self.settings.desk_mic = desk_mic;
                        self.mark_dirty();
                    }
                },
            );
            theme::separator(ui);
            let busy = self.jobs.setup.is_some();
            theme::row(
                ui,
                "Clean Mic",
                if attached_here {
                    "Installed on this microphone."
                } else {
                    "Not installed on this microphone yet."
                },
                |ui| {
                    if busy {
                        ui.spinner();
                    } else if attached_here {
                        if theme::destructive_button(ui, "Remove").clicked() {
                            self.run_setup(vec!["uninstall".into()]);
                        }
                    } else if theme::primary_button(ui, "Install").clicked() {
                        let args =
                            vec!["install".into(), "--mic".into(), self.selected_mic.clone()];
                        self.run_setup(args);
                    }
                },
            );
            if let Some(message) = &self.jobs.setup_message {
                theme::hint(ui, message);
            }
        });
        if refresh {
            self.refresh_mics();
        }
        if let Some(warning) = self.level_warning {
            ui.add_space(12.0);
            let p = palette(ui);
            let message = match warning {
                level_watch::LevelWarning::Clipping => "Your microphone is clipping. Lower its input level in Windows Sound settings (or the mic's own gain) until loud words stay below the top.",
                level_watch::LevelWarning::TooQuiet => "Your microphone is very quiet. Raise its input level in Windows Sound settings so Hush can tell your voice from noise.",
            };
            theme::card(ui, None, None, |ui| {
                ui.horizontal(|ui| {
                    let text_width = (ui.available_width() - 130.0).max(150.0);
                    ui.vertical(|ui| {
                        ui.set_width(text_width);
                        ui.label(RichText::new(message).small().color(p.warning));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::outline_button(ui, "Sound settings").clicked() {
                            open_in_shell("ms-settings:sound");
                        }
                    });
                });
            });
        }
        ui.add_space(12.0);
        theme::card(ui, Some("How it works"), None, |ui| {
            theme::hint(
                ui,
                "Clean Mic runs inside Windows' own audio engine, so it works in every app, even when this window is closed.",
            );
            theme::hint(
                ui,
                &format!(
                    "In Discord and games, select \"{}\" directly, not another app's virtual microphone (such as SteelSeries Sonar), or your voice will be processed twice.",
                    if self.selected_mic.is_empty() { "your microphone" } else { &self.selected_mic }
                ),
            );
            theme::hint(
                ui,
                "Installing asks for administrator permission once and restarts Windows audio for a moment.",
            );
        });
        if attached_here && self.state.glitches > 0 {
            ui.add_space(12.0);
            let p = palette(ui);
            theme::card(ui, Some("Audio gaps detected"), None, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} short gaps since Windows audio started. If you hear crackles, close heavy background apps and tell us.",
                        self.state.glitches
                    ))
                    .small()
                    .color(p.warning),
                );
            });
        }
    }
}
