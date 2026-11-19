//! Settings page: startup, health, mute, tuning snapshot and support.

use super::super::HushApp;
use crate::{
    prefs, settings_io,
    theme::{self, palette},
};
use eframe::egui::{self, RichText};
use hush_apo::{dsp::MicSettings, settings};
use std::time::{Duration, Instant, SystemTime};

impl HushApp {
    pub(in crate::ui) fn settings_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(ui, "Settings", "How Hush starts and behaves.");
        let p = palette(ui);
        theme::card(ui, Some("Startup"), None, |ui| {
            theme::row(
                ui,
                "Start with Windows",
                "Starts quietly in the system tray when you sign in.",
                |ui| {
                    if theme::switch(ui, &mut self.start_with_windows, "Start with Windows")
                        .changed()
                    {
                        self.prefs_error = prefs::set_start_with_windows(self.start_with_windows)
                            .err()
                            .map(|e| format!("Could not change startup setting: {e}"));
                        self.start_with_windows = prefs::start_with_windows();
                    }
                },
            );
            theme::separator(ui);
            theme::row(
                ui,
                "Keep running in the tray",
                "Closing the window hides it to the tray instead of quitting.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.close_to_tray, "Close to tray").changed() {
                        self.prefs_error = prefs::save(&self.prefs)
                            .err()
                            .map(|e| format!("Could not save: {e}"));
                    }
                },
            );
            if let Some(error) = &self.prefs_error {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Microphone health"), None, |ui| {
            theme::row(
                ui,
                "Repair Clean Mic automatically",
                "If your mic reconnects without Clean Mic, Hush puts it back. This can briefly restart Windows audio.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.auto_repair, "Repair automatically")
                        .changed()
                    {
                        self.save_prefs();
                    }
                },
            );
            theme::separator(ui);
            theme::row(
                ui,
                "Notifications",
                "Tells you when your microphone disconnects or Clean Mic stops working.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.notifications, "Notifications").changed() {
                        self.save_prefs();
                    }
                },
            );
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Mute"), None, |ui| {
            theme::row(
                ui,
                "Mute hotkey",
                "Mutes your microphone in every app. Hush listens only for this key.",
                |ui| {
                    if theme::switch(ui, &mut self.prefs.mute_hotkey, "Mute hotkey").changed() {
                        self.save_prefs();
                        self.apply_hotkey();
                    }
                },
            );
            if self.prefs.mute_hotkey {
                theme::separator(ui);
                theme::row_sized(ui, "Key", "", 160.0, |ui| {
                    let mut key = self.prefs.mute_key;
                    egui::ComboBox::from_id_salt("mute-key")
                        .selected_text(key.label())
                        .show_ui(ui, |ui| {
                            for option in prefs::MuteKey::ALL {
                                ui.selectable_value(&mut key, option, option.label());
                            }
                        });
                    if key != self.prefs.mute_key {
                        self.prefs.mute_key = key;
                        self.save_prefs();
                        self.apply_hotkey();
                    }
                });
            }
            if let Some(error) = &self.hotkey_error {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
            theme::separator(ui);
            let muted = self.mute.is_muted();
            theme::row(
                ui,
                "Microphone",
                if self.selected_mic.is_empty() {
                    "Uses the Windows mute switch."
                } else {
                    "Uses the Windows mute switch of the selected microphone."
                },
                |ui| {
                    if muted {
                        if theme::outline_button(ui, "Unmute").clicked() {
                            self.mute.toggle();
                        }
                        theme::badge(ui, p.destructive, "Muted");
                    } else if theme::outline_button(ui, "Mute").clicked() {
                        self.mute.toggle();
                    }
                },
            );
            if let Some(error) = self.mute.error() {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Microphone settings"), None, |ui| {
            theme::row_sized(
                ui,
                "Reset to defaults",
                "Restores noise, gate and voice settings.",
                190.0,
                |ui| {
                    let armed = self
                        .reset_armed
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(4));
                    if armed {
                        ui.ctx().request_repaint_after(Duration::from_millis(250));
                        if theme::destructive_button(ui, "Click again to reset").clicked() {
                            self.settings = MicSettings::default();
                            self.mark_dirty();
                            self.reset_armed = None;
                        }
                    } else if theme::outline_button(ui, "Reset").clicked() {
                        self.reset_armed = Some(Instant::now());
                    }
                },
            );
            theme::separator(ui);
            let snapshot_note = match self.snapshot_saved {
                Some(saved) => format!(
                    "Saved {}. Restoring keeps Clean Mic on or off as it is.",
                    settings_io::ago(
                        SystemTime::now()
                            .duration_since(saved)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    )
                ),
                None => "Keep a copy of your noise, gate and voice settings to come back to."
                    .to_string(),
            };
            theme::row_sized(ui, "Tuning snapshot", &snapshot_note, 250.0, |ui| {
                if ui
                    .add_enabled_ui(self.snapshot_saved.is_some(), |ui| {
                        theme::outline_button(ui, "Restore tuning")
                    })
                    .inner
                    .clicked()
                {
                    self.tuning_message = Some(match settings_io::load_snapshot() {
                        Ok(snapshot) => {
                            self.apply_loaded_settings(snapshot);
                            ("Restored your saved tuning.".to_string(), true)
                        }
                        Err(message) => (message, false),
                    });
                }
                if theme::outline_button(ui, "Save tuning").clicked() {
                    self.tuning_message = Some(match settings_io::save_snapshot(&self.settings) {
                        Ok(()) => {
                            self.snapshot_saved = settings_io::snapshot_time();
                            ("Saved your tuning.".to_string(), true)
                        }
                        Err(e) => (format!("Could not save tuning: {e}"), false),
                    });
                }
            });
            theme::separator(ui);
            theme::row_sized(
                ui,
                "Export or import",
                "Share your settings as a .conf file.",
                190.0,
                |ui| {
                    ui.add_enabled_ui(self.jobs.file.is_none(), |ui| {
                        if theme::outline_button(ui, "Import\u{2026}").clicked() {
                            self.jobs.file = Some(settings_io::import(self.wake()));
                        }
                        if theme::outline_button(ui, "Export\u{2026}").clicked() {
                            self.jobs.file = Some(settings_io::export(self.settings, self.wake()));
                        }
                    });
                },
            );
            if let Some((message, ok)) = &self.tuning_message {
                let color = if *ok { p.success } else { p.destructive };
                ui.label(RichText::new(message).small().color(color));
            }
            theme::separator(ui);
            theme::row(
                ui,
                "Settings folder",
                "Where mic settings and diagnostics are stored.",
                |ui| {
                    if theme::outline_button(ui, "Open").clicked() {
                        let _ = std::process::Command::new("explorer.exe")
                            .arg(settings::settings_dir())
                            .spawn();
                    }
                },
            );
            if let Some(error) = &self.save_error {
                ui.label(RichText::new(error).small().color(p.destructive));
            }
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Support"), None, |ui| {
            theme::row_sized(
                ui,
                "Diagnostics",
                "Copies a text summary to paste into a bug report. No audio is included.",
                160.0,
                |ui| self.diagnostics_button(ui),
            );
        });
    }
}
