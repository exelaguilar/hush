//! About page: what is inside, status and help links.

use super::super::{open_in_shell, HushApp};
use crate::{
    theme::{self, palette},
    updates,
};
use eframe::egui::{self, RichText};

impl HushApp {
    pub(in crate::ui) fn about_page(&mut self, ui: &mut egui::Ui) {
        theme::page_header(ui, "About", &format!("Hush {}", env!("CARGO_PKG_VERSION")));
        theme::card(ui, Some("What's inside"), None, |ui| {
            theme::hint(
                ui,
                "Noise removal: DeepFilterNet 3 (MIT / Apache-2.0), running on your CPU.",
            );
            theme::hint(ui, "Audio plugin: runs inside Windows' audio engine (audiodg.exe) on the selected microphone.");
            theme::hint(
                ui,
                "No virtual devices and no kernel drivers are installed.",
            );
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Status"), None, |ui| {
            let attached = if self.state.attached.is_empty() {
                "none".to_string()
            } else {
                self.state.attached.join(", ")
            };
            theme::hint(ui, &format!("Installed on: {attached}"));
            theme::hint(
                ui,
                &format!(
                    "Audio engine: {}",
                    if self.state.running {
                        "processing"
                    } else {
                        "idle"
                    }
                ),
            );
            theme::hint(
                ui,
                &format!(
                    "Audio gaps since Windows audio started: {}",
                    self.state.glitches
                ),
            );
            ui.add_space(4.0);
            self.diagnostics_button(ui);
        });
        ui.add_space(12.0);
        theme::card(ui, Some("Help"), None, |ui| {
            let program_dir = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()));
            let notices = program_dir
                .as_ref()
                .map(|dir| dir.join("THIRD-PARTY-NOTICES.txt"))
                .filter(|path| path.exists());
            let repository = env!("CARGO_PKG_REPOSITORY");
            ui.horizontal(|ui| {
                if !repository.is_empty() && theme::outline_button(ui, "Report a problem").clicked()
                {
                    open_in_shell(&format!("{repository}/issues"));
                }
                if let Some(notices) = &notices {
                    if theme::outline_button(ui, "Licenses").clicked() {
                        open_in_shell(&notices.display().to_string());
                    }
                }
                // Only offered when the build knows its GitHub repository.
                if updates::parse_repository(repository).is_some()
                    && self.jobs.update.is_none()
                    && theme::outline_button(ui, "Check for updates").clicked()
                {
                    self.jobs.update_result = None;
                    self.jobs.update = updates::check(repository);
                }
            });
            if updates::parse_repository(repository).is_some() {
                if self.jobs.update.is_some() {
                    theme::hint(ui, "Checking\u{2026}");
                }
                match &self.jobs.update_result {
                    Some(Ok(updates::Outcome::UpToDate)) => {
                        ui.label(
                            RichText::new("You have the latest version.")
                                .small()
                                .color(palette(ui).success),
                        );
                    }
                    Some(Ok(updates::Outcome::Available { version, url })) => {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("Hush {version} is available."))
                                    .small()
                                    .color(palette(ui).success),
                            );
                            if theme::outline_button(ui, "Open release page").clicked() {
                                open_in_shell(url);
                            }
                        });
                    }
                    Some(Err(error)) => {
                        ui.label(RichText::new(error).small().color(palette(ui).destructive));
                    }
                    None => {}
                }
                theme::hint(
                    ui,
                    "Checks GitHub once when you click. Hush never checks on its own.",
                );
            }
            theme::hint(
                ui,
                "To uninstall Hush completely, use Windows Settings > Apps > Installed apps.",
            );
        });
    }
}
