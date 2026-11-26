//! shadcn/ui-inspired look for egui: neutral zinc palette, 1 px borders,
//! 6-8 px rounding, teal-blue primary, Segoe UI. Follows Windows light/dark.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Response,
    RichText, Sense, Stroke, StrokeKind, TextStyle, Theme, Ui, Vec2,
};
use std::sync::Arc;

pub const TEAL: Color32 = Color32::from_rgb(20, 184, 196);
const TEAL_LIGHT_MODE: Color32 = Color32::from_rgb(13, 148, 160);

/// Semantic colours for the current theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Color32,
    pub sidebar: Color32,
    pub card: Color32,
    pub muted: Color32,
    /// Empty part of a slider rail, meter or switch (visible on the card).
    pub track: Color32,
    pub border: Color32,
    pub foreground: Color32,
    pub muted_foreground: Color32,
    pub primary: Color32,
    pub primary_foreground: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub destructive: Color32,
}

pub const DARK: Palette = Palette {
    background: Color32::from_rgb(9, 9, 11),
    sidebar: Color32::from_rgb(12, 12, 15),
    card: Color32::from_rgb(14, 14, 17),
    muted: Color32::from_rgb(39, 39, 42),
    track: Color32::from_rgb(39, 39, 42),
    border: Color32::from_rgb(39, 39, 42),
    foreground: Color32::from_rgb(250, 250, 250),
    muted_foreground: Color32::from_rgb(161, 161, 170),
    primary: TEAL,
    primary_foreground: Color32::from_rgb(4, 30, 33),
    success: Color32::from_rgb(34, 197, 94),
    warning: Color32::from_rgb(245, 158, 11),
    destructive: Color32::from_rgb(239, 68, 68),
};

pub const LIGHT: Palette = Palette {
    background: Color32::from_rgb(255, 255, 255),
    sidebar: Color32::from_rgb(250, 250, 250),
    card: Color32::from_rgb(255, 255, 255),
    muted: Color32::from_rgb(240, 240, 242),
    track: Color32::from_rgb(212, 212, 216),
    border: Color32::from_rgb(228, 228, 231),
    foreground: Color32::from_rgb(9, 9, 11),
    muted_foreground: Color32::from_rgb(113, 113, 122),
    primary: TEAL_LIGHT_MODE,
    primary_foreground: Color32::WHITE,
    success: Color32::from_rgb(22, 163, 74),
    warning: Color32::from_rgb(180, 83, 9),
    destructive: Color32::from_rgb(220, 38, 38),
};

pub fn palette(ui: &Ui) -> Palette {
    if ui.visuals().dark_mode {
        DARK
    } else {
        LIGHT
    }
}

pub const SEMIBOLD: &str = "semibold";

fn semibold() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

/// Segoe UI (Windows' UI font) when available; egui's bundled fonts remain
/// as fallbacks for symbols.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let read = |name: &str| std::fs::read(format!(r"C:\Windows\Fonts\{name}")).ok();
    if let Some(regular) = read("segoeui.ttf") {
        fonts
            .font_data
            .insert("segoe".into(), Arc::new(FontData::from_owned(regular)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "segoe".into());
    }
    let mut semibold_stack = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if let Some(bold) = read("seguisb.ttf") {
        fonts.font_data.insert(
            "segoe-semibold".into(),
            Arc::new(FontData::from_owned(bold)),
        );
        semibold_stack.insert(0, "segoe-semibold".into());
    }
    fonts.families.insert(semibold(), semibold_stack);
    ctx.set_fonts(fonts);
}

fn visuals_for(p: Palette, dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let radius = CornerRadius::same(6);
    v.panel_fill = p.background;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = p.background;
    v.faint_bg_color = p.muted;
    v.code_bg_color = p.muted;
    v.hyperlink_color = p.primary;
    v.selection.bg_fill = p.primary;
    v.selection.stroke = Stroke::new(1.0, p.primary_foreground);
    v.override_text_color = Some(p.foreground);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.foreground);
    w.noninteractive.corner_radius = radius;
    // Outline-style controls at rest; slider rails use `bg_fill`.
    w.inactive.bg_fill = p.track;
    w.inactive.weak_bg_fill = p.background;
    w.inactive.bg_stroke = Stroke::new(1.0, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0, p.foreground);
    w.inactive.corner_radius = radius;
    w.hovered.bg_fill = p.track;
    w.hovered.weak_bg_fill = p.muted;
    w.hovered.bg_stroke = Stroke::new(1.0, p.border);
    w.hovered.fg_stroke = Stroke::new(1.0, p.foreground);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = p.track;
    w.active.weak_bg_fill = p.muted;
    w.active.bg_stroke = Stroke::new(1.0, p.primary);
    w.active.fg_stroke = Stroke::new(1.0, p.foreground);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;
    w.open = w.active;
    v
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::ThemePreference::System);
    for (theme, palette, dark) in [(Theme::Dark, DARK, true), (Theme::Light, LIGHT, false)] {
        ctx.style_mut_of(theme, |style| {
            style.visuals = visuals_for(palette, dark);
            style.spacing.item_spacing = Vec2::new(8.0, 8.0);
            style.spacing.button_padding = Vec2::new(14.0, 7.0);
            style.spacing.interact_size.y = 30.0;
            // Slider value boxes share one width so the rails line up.
            style.spacing.interact_size.x = 76.0;
            style.spacing.slider_width = 240.0;
            style.spacing.combo_width = 260.0;
            style.text_styles = [
                (TextStyle::Heading, FontId::new(24.0, semibold())),
                (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
                (
                    TextStyle::Button,
                    FontId::new(14.0, FontFamily::Proportional),
                ),
                (
                    TextStyle::Small,
                    FontId::new(12.5, FontFamily::Proportional),
                ),
                (
                    TextStyle::Monospace,
                    FontId::new(13.0, FontFamily::Monospace),
                ),
            ]
            .into();
        });
    }
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// Page title with a muted description underneath.
pub fn page_header(ui: &mut Ui, title: &str, description: &str) {
    let p = palette(ui);
    ui.add_space(10.0);
    ui.label(RichText::new(title).heading());
    ui.add_space(2.0);
    ui.label(RichText::new(description).color(p.muted_foreground));
    ui.add_space(18.0);
}

/// Bordered card with optional title and description.
pub fn card<R>(
    ui: &mut Ui,
    title: Option<&str>,
    description: Option<&str>,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let p = palette(ui);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if let Some(title) = title {
                ui.label(RichText::new(title).family(semibold()).size(15.5));
            }
            if let Some(description) = description {
                ui.label(RichText::new(description).color(p.muted_foreground).small());
            }
            if title.is_some() || description.is_some() {
                ui.add_space(12.0);
            }
            add(ui)
        })
        .inner
}

/// Settings row: title + muted description on the left, a small control
/// (switch, button) on the right.
pub fn row<R>(
    ui: &mut Ui,
    title: &str,
    description: &str,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    row_sized(ui, title, description, 140.0, control)
}

/// Width reserved for a slider with its value box.
pub const SLIDER_WIDTH: f32 = 340.0;
/// Width reserved for a drop-down.
pub const COMBO_WIDTH: f32 = 290.0;

/// Settings row reserving `control_width` on the right for the control.
pub fn row_sized<R>(
    ui: &mut Ui,
    title: &str,
    description: &str,
    control_width: f32,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    let p = palette(ui);
    let label = |ui: &mut Ui| {
        ui.label(RichText::new(title).family(semibold()));
        if !description.is_empty() {
            ui.label(RichText::new(description).color(p.muted_foreground).small());
        }
    };
    if ui.available_width() < control_width + 162.0 {
        label(ui);
        ui.add_space(4.0);
        let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
        return ui
            .allocate_ui_with_layout(
                size,
                egui::Layout::right_to_left(egui::Align::Center),
                control,
            )
            .inner;
    }
    ui.horizontal(|ui| {
        let text_width = ui.available_width() - control_width - 12.0;
        ui.vertical(|ui| {
            ui.set_width(text_width);
            label(ui);
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control)
            .inner
    })
    .inner
}

pub fn separator(ui: &mut Ui) {
    let p = palette(ui);
    ui.add_space(6.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.border);
    ui.add_space(6.0);
}

/// shadcn-style switch. Returns a response that is `changed()` on toggle.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let p = palette(ui);
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let enabled = ui.is_enabled();
    let track = if *on { p.primary } else { p.track };
    let track = if enabled {
        track
    } else {
        track.gamma_multiply(0.5)
    };
    let radius = rect.height() / 2.0;
    ui.painter().rect(
        rect,
        radius,
        track,
        if enabled && !*on {
            Stroke::new(1.0, p.muted_foreground)
        } else {
            Stroke::NONE
        },
        StrokeKind::Inside,
    );
    let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    let knob = if ui.visuals().dark_mode && !*on {
        p.foreground.gamma_multiply(0.9)
    } else {
        Color32::WHITE
    };
    ui.painter()
        .circle_filled(egui::pos2(knob_x, rect.center().y), radius - 3.0, knob);
    // Keyboard focus (Tab, then Space or Enter toggles it).
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(3.0),
            radius + 3.0,
            Stroke::new(2.0, p.primary),
            StrokeKind::Outside,
        );
    }
    // Screen readers announce it as an on/off switch.
    let state = *on;
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, enabled, state, label));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

pub const MIC_POSITION_WIDTH: f32 = 136.0;

/// Setup can also leave the previously saved position alone.
pub fn mic_position(ui: &mut Ui, desk_mic: &mut Option<bool>, keep_saved: bool) {
    egui::ComboBox::from_id_salt("mic_position")
        .width(MIC_POSITION_WIDTH)
        .selected_text(match desk_mic {
            Some(true) => "On the desk",
            Some(false) => "Near my mouth",
            None => "Keep saved position",
        })
        .show_ui(ui, |ui| {
            if keep_saved {
                ui.selectable_value(desk_mic, None, "Keep saved position");
            }
            ui.selectable_value(desk_mic, Some(false), "Near my mouth");
            ui.selectable_value(desk_mic, Some(true), "On the desk");
        });
}

/// Filled teal button (one per screen, for the main action).
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .color(p.primary_foreground)
                .family(semibold()),
        )
        .fill(p.primary)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(6)),
    )
}

/// Outline button.
pub fn outline_button(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.add(
        egui::Button::new(text)
            .fill(p.background)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(6)),
    )
}

/// Destructive outline button (e.g. Remove).
pub fn destructive_button(ui: &mut Ui, text: &str) -> Response {
    let p = palette(ui);
    ui.add(
        egui::Button::new(RichText::new(text).color(p.destructive))
            .fill(p.background)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(6)),
    )
}

/// Small pill with a coloured dot, e.g. "● Running".
pub fn badge(ui: &mut Ui, color: Color32, text: &str) -> egui::Response {
    let p = palette(ui);
    egui::Frame::new()
        .fill(
            p.muted
                .gamma_multiply(if ui.visuals().dark_mode { 0.6 } else { 1.0 }),
        )
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (dot, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.5, color);
                ui.label(RichText::new(text).small().family(semibold()));
            });
        })
        .response
}

/// Muted helper text.
pub fn hint(ui: &mut Ui, text: &str) {
    let p = palette(ui);
    ui.label(RichText::new(text).small().color(p.muted_foreground));
}

pub fn semibold_text(text: &str) -> RichText {
    RichText::new(text).family(semibold())
}
