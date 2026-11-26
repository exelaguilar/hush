//! Window chrome shared by every page: sidebar, header, health banner,
//! the "Hold to hear original" button and the recording toast.

use super::{open_in_shell, HushApp, Page};
use crate::{
    health,
    theme::{self, palette},
};
use eframe::egui::{self, RichText};
use std::time::Duration;

impl HushApp {
    pub(in crate::ui) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let logo = self.logo.get_or_insert_with(|| {
                ui.ctx().load_texture(
                    "hush-logo",
                    egui::ColorImage::from_rgba_unmultiplied(
                        [64, 64],
                        &crate::icon_art::render_icon(64),
                    ),
                    egui::TextureOptions::LINEAR,
                )
            });
            ui.add(egui::Image::new((logo.id(), egui::vec2(28.0, 28.0))));
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(theme::semibold_text("Hush").size(17.0));
                ui.label(
                    RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                        .size(11.0)
                        .color(p.muted_foreground),
                );
            });
        });
        ui.add_space(18.0);
        let attention = self.health.current().needs_attention();
        let mut nav = |ui: &mut egui::Ui, page: Page| {
            let selected = self.page == page;
            let text = RichText::new(page.label()).color(if selected {
                p.foreground
            } else {
                p.muted_foreground
            });
            let button = egui::Button::selectable(selected, text)
                .fill(if selected {
                    p.muted
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(egui::Stroke::NONE)
                .corner_radius(6)
                .min_size(egui::vec2(ui.available_width(), 34.0));
            let mut response = ui
                .add(button)
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if selected {
                let r = response.rect;
                let bar = egui::Rect::from_min_size(
                    egui::pos2(r.left(), r.center().y - 8.0),
                    egui::vec2(3.0, 16.0),
                );
                ui.painter().rect_filled(bar, 2.0, p.primary);
            }
            if page == Page::Microphone && attention {
                // The microphone needs attention: a dot leads there.
                let r = response.rect;
                ui.painter().circle_filled(
                    egui::pos2(r.right() - 14.0, r.center().y),
                    4.0,
                    p.warning,
                );
                response = response.on_hover_text(self.health.current().short());
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::Role::Button,
                        true,
                        selected,
                        format!("Microphone, {}", self.health.current().short()),
                    )
                });
            }
            if response.clicked() {
                self.page = page;
            }
        };
        for page in Page::MAIN {
            nav(ui, page);
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(12.0);
            for page in Page::FOOTER.iter().rev() {
                nav(ui, *page);
            }
        });
    }

    pub(in crate::ui) fn header(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let (color, text) = self.status();
            ui.label(theme::semibold_text("Clean Mic").size(16.0));
            // "Active · Apex Legends, Discord": the apps that get the cleaned mic.
            match self.apps_label().filter(|_| text == "Active") {
                Some(apps) => {
                    theme::badge(ui, color, &format!("Active · {apps}"))
                        .on_hover_text(self.health.agent.apps.join(", "));
                }
                None => {
                    theme::badge(ui, color, text);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::switch(ui, &mut self.settings.enabled, "Clean Mic")
                    .on_hover_text("Turn microphone processing on or off")
                    .changed()
                {
                    self.mark_dirty();
                }
            });
        });
        let talking = self
            .live_levels
            .is_some_and(|levels| levels.voice && levels.age_ms() < 1500 && self.settings.enabled);
        let muted = self.mute.is_muted();
        let health = self.health.current();
        if talking || muted || health.needs_attention() {
            ui.horizontal_wrapped(|ui| {
                if talking {
                    theme::badge(ui, p.primary, "Talking");
                }
                if muted {
                    theme::badge(ui, p.destructive, "Muted");
                }
                if health.needs_attention() {
                    theme::badge(ui, p.warning, health.short());
                }
            });
        }
        ui.add_space(6.0);
    }

    pub(in crate::ui) fn health_banner(&mut self, ui: &mut egui::Ui) {
        let fixing = self.health.repairing();
        let current = self.health.current().clone();
        if current == health::Health::Healthy && !fixing {
            return;
        }
        let p = palette(ui);
        let informational = !current.needs_attention();
        let message = if fixing {
            "Fixing Clean Mic…".to_string()
        } else {
            current.message(self.health.target())
        };
        let fix = current.fix().filter(|_| !fixing);
        let sound_settings = current == health::Health::EnhancementsOff && !fixing;
        let mut clicked = false;
        theme::card(ui, None, None, |ui| {
            ui.horizontal(|ui| {
                let text_width = (ui.available_width() - 110.0).max(150.0);
                ui.vertical(|ui| {
                    ui.set_width(text_width);
                    let color = if informational {
                        p.muted_foreground
                    } else {
                        p.warning
                    };
                    ui.label(RichText::new(message).small().color(color));
                    if let Some(note) = self.health.note() {
                        theme::hint(ui, note);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if fixing {
                        ui.spinner();
                    } else if sound_settings {
                        if theme::outline_button(ui, "Sound settings").clicked() {
                            open_in_shell("ms-settings:sound");
                        }
                    } else if fix.is_some() && theme::outline_button(ui, "Fix now").clicked() {
                        clicked = true;
                    }
                });
            });
        });
        ui.add_space(12.0);
        if let (true, Some(kind)) = (clicked, fix) {
            // The user asked for it, so the UAC fallback is allowed.
            self.fix_now(kind);
        }
    }

    /// "Hold to hear original" button. Returns while pressed.
    pub(in crate::ui) fn compare_button(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let label = if self.compare.is_some() {
            theme::semibold_text("Hearing original…").color(p.primary_foreground)
        } else {
            RichText::new("Hold to hear original")
        };
        let response = ui.add(
            egui::Button::new(label)
                .fill(if self.compare.is_some() {
                    p.primary
                } else {
                    p.background
                })
                .stroke(egui::Stroke::new(
                    1.0,
                    if self.compare.is_some() {
                        p.primary
                    } else {
                        p.border
                    },
                ))
                .corner_radius(6)
                .sense(egui::Sense::click_and_drag()),
        );
        let held = response.is_pointer_button_down_on()
            || (response.has_focus()
                && ui.input(|input| {
                    input.key_down(egui::Key::Space) || input.key_down(egui::Key::Enter)
                }));
        match (held, self.compare.is_some()) {
            (true, false) => match hush_apo::settings::CompareLease::begin() {
                Ok(lease) => self.compare = Some((lease, std::time::Instant::now(), self.page)),
                Err(error) => self.save_error = Some(format!("Could not compare: {error}")),
            },
            (false, true) => self.compare = None,
            _ => {}
        }
    }

    pub(in crate::ui) fn diagnostics_button(&mut self, ui: &mut egui::Ui) {
        let copied = self
            .diag_copied
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        let label = if copied { "Copied" } else { "Copy diagnostics" };
        if theme::outline_button(ui, label).clicked() {
            self.copy_diagnostics();
        }
    }

    /// Toast in the bottom-right corner after "Save" in Mic Test: the file
    /// path with Copy / Show in folder. Closes itself after 10 s unless the
    /// pointer is over it.
    pub(in crate::ui) fn recording_toast(&mut self, ctx: &egui::Context) {
        let Some((result, shown)) = &self.test_state.saved else {
            return;
        };
        let p = if ctx.global_style().visuals.dark_mode {
            theme::DARK
        } else {
            theme::LIGHT
        };
        let mut close = false;
        let response = egui::Area::new(egui::Id::unique("recording-toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-20.0, -20.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(p.card)
                    .stroke(egui::Stroke::new(1.0, p.border))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(14))
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 16,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(80),
                    })
                    .show(ui, |ui| {
                        ui.set_max_width(380.0);
                        ui.horizontal(|ui| {
                            let title = if result.is_ok() {
                                "Recording saved"
                            } else {
                                "Could not save recording"
                            };
                            ui.label(theme::semibold_text(title));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(egui::Button::new("×").frame(false))
                                        .on_hover_text("Close")
                                        .clicked()
                                    {
                                        close = true;
                                    }
                                },
                            );
                        });
                        match result {
                            Ok(path) => {
                                let text = path.display().to_string();
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&text).small().color(p.muted_foreground),
                                    )
                                    .wrap(),
                                );
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    if theme::primary_button(ui, "Copy path").clicked() {
                                        close = crate::clipboard::copy(&text);
                                    }
                                    if theme::outline_button(ui, "Show in folder").clicked() {
                                        let _ = std::process::Command::new("explorer.exe")
                                            .arg(format!("/select,{text}"))
                                            .spawn();
                                    }
                                });
                            }
                            Err(error) => {
                                ui.label(RichText::new(error).small().color(p.destructive));
                            }
                        }
                    });
            })
            .response;
        let hovered = response.contains_pointer();
        if close || (!hovered && shown.elapsed() > Duration::from_secs(10)) {
            self.test_state.saved = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}
