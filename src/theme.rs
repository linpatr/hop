//! Look of the Hop window: colors for light and dark mode, and a few widgets (cards, a switch,
//! the accent button, status pills).

use std::sync::Arc;

use eframe::egui::{self, Color32, CornerRadius, FontData, FontFamily, FontId, Margin, RichText, Stroke, TextStyle, Theme, Vec2};

/// Brand accent, between the blue and purple of the app icon.
pub const ACCENT: Color32 = Color32::from_rgb(92, 106, 255);
const ACCENT_HOVER: Color32 = Color32::from_rgb(116, 128, 255);
pub const GREEN: Color32 = Color32::from_rgb(46, 184, 114);
pub const AMBER: Color32 = Color32::from_rgb(226, 158, 52);
pub const RED: Color32 = Color32::from_rgb(229, 72, 77);

struct Palette {
    bg: Color32,
    card: Color32,
    card_stroke: Color32,
    field: Color32,
    text: Color32,
    weak: Color32,
    widget: Color32,
    widget_hover: Color32,
}

const DARK: Palette = Palette {
    bg: Color32::from_rgb(20, 21, 26),
    card: Color32::from_rgb(30, 32, 40),
    card_stroke: Color32::from_rgb(44, 47, 58),
    field: Color32::from_rgb(22, 23, 29),
    text: Color32::from_rgb(232, 233, 239),
    weak: Color32::from_rgb(139, 143, 163),
    widget: Color32::from_rgb(44, 47, 58),
    widget_hover: Color32::from_rgb(56, 60, 74),
};

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(243, 244, 248),
    card: Color32::from_rgb(255, 255, 255),
    card_stroke: Color32::from_rgb(226, 228, 236),
    field: Color32::from_rgb(246, 247, 250),
    text: Color32::from_rgb(27, 28, 34),
    weak: Color32::from_rgb(107, 112, 128),
    widget: Color32::from_rgb(234, 236, 242),
    widget_hover: Color32::from_rgb(222, 225, 234),
};

fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// Background behind the cards.
pub fn background(ctx: &egui::Context) -> Color32 {
    palette(ctx.theme() == Theme::Dark).bg
}

pub fn weak(ui: &egui::Ui) -> Color32 {
    palette(ui.visuals().dark_mode).weak
}

fn style_for(theme: Theme) -> egui::Style {
    let dark = theme == Theme::Dark;
    let p = palette(dark);
    let mut style = egui::Style { visuals: if dark { egui::Visuals::dark() } else { egui::Visuals::light() }, ..Default::default() };
    style.text_styles = [
        (TextStyle::Heading, FontId::proportional(22.0)),
        (TextStyle::Body, FontId::proportional(14.0)),
        (TextStyle::Button, FontId::proportional(14.0)),
        (TextStyle::Small, FontId::proportional(12.0)),
        (TextStyle::Monospace, FontId::monospace(14.0)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(10.0, 8.0);
    s.button_padding = Vec2::new(14.0, 6.0);
    s.interact_size.y = 30.0;
    s.window_margin = Margin::same(16);

    let v = &mut style.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.extreme_bg_color = p.field;
    v.faint_bg_color = p.field;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT.linear_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_stroke = Stroke::new(1.0, p.card_stroke);
    for (w, fill) in [
        (&mut v.widgets.inactive, p.widget),
        (&mut v.widgets.hovered, p.widget_hover),
        (&mut v.widgets.active, p.widget_hover),
        (&mut v.widgets.open, p.widget_hover),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::NONE;
        w.corner_radius = CornerRadius::same(8);
        w.fg_stroke.color = p.text;
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.card_stroke);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.card_stroke);
    v.widgets.noninteractive.fg_stroke.color = p.text;
    v.widgets.noninteractive.corner_radius = CornerRadius::same(8);
    style
}

/// The system's own UI fonts (regular, bold, monospace), so Hop looks native. Each is optional;
/// egui's built-in fonts stay as fallbacks (they also provide the emoji icons).
fn system_fonts() -> [Option<(&'static str, u32)>; 3] {
    if cfg!(target_os = "macos") {
        [
            Some(("/System/Library/Fonts/SFNS.ttf", 0)),
            Some(("/System/Library/Fonts/HelveticaNeue.ttc", 10)), // Medium
            Some(("/System/Library/Fonts/Menlo.ttc", 0)),
        ]
    } else if cfg!(windows) {
        [
            Some((r"C:\Windows\Fonts\segoeui.ttf", 0)),
            Some((r"C:\Windows\Fonts\seguisb.ttf", 0)), // Semibold
            Some((r"C:\Windows\Fonts\consola.ttf", 0)),
        ]
    } else {
        [None, None, None]
    }
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let proportional = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut bold = proportional.clone();
    for (i, f) in system_fonts().into_iter().enumerate() {
        let Some((path, index)) = f else { continue };
        let Ok(bytes) = std::fs::read(path) else { continue };
        let name = ["ui", "ui-bold", "ui-mono"][i].to_string();
        fonts.font_data.insert(name.clone(), Arc::new(FontData { index, ..FontData::from_owned(bytes) }));
        match i {
            0 => fonts.families.entry(FontFamily::Proportional).or_default().insert(0, name),
            1 => bold.insert(0, name),
            _ => fonts.families.entry(FontFamily::Monospace).or_default().insert(0, name),
        }
    }
    if bold.len() == proportional.len()
        && let Some(ui) = fonts.families.get(&FontFamily::Proportional).and_then(|f| f.first()).cloned()
    {
        bold.insert(0, ui);
    }
    fonts.families.insert(FontFamily::Name("bold".into()), bold);
    ctx.set_fonts(fonts);
}

/// Text in the bold UI font.
pub fn bold(text: impl Into<String>) -> RichText {
    RichText::new(text).family(FontFamily::Name("bold".into()))
}

/// Status colors, darker in light mode so they stay readable.
pub fn green(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { GREEN } else { Color32::from_rgb(20, 140, 78) }
}

pub fn amber(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { AMBER } else { Color32::from_rgb(168, 100, 8) }
}

/// Applies Hop's look; light or dark follows the system.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::System);
    ctx.set_style_of(Theme::Dark, style_for(Theme::Dark));
    ctx.set_style_of(Theme::Light, style_for(Theme::Light));
}

/// A rounded panel grouping related things.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = palette(ui.visuals().dark_mode);
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(14)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Small caps label above a card's contents.
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(bold(text.to_uppercase()).size(11.5).color(weak(ui)));
}

/// The main call-to-action button.
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let b = egui::Button::new(bold(text).color(Color32::WHITE)).fill(ACCENT).corner_radius(8);
    let r = ui.add(b);
    if r.hovered() {
        ui.painter().rect_filled(r.rect, 8, ACCENT_HOVER.linear_multiply(0.18));
    }
    r
}

/// An on/off switch.
pub fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = Vec2::new(ui.spacing().interact_size.y * 1.7, ui.spacing().interact_size.y * 0.85);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool(response.id, *on);
        let off = palette(ui.visuals().dark_mode).widget_hover;
        let fill = Color32::from_rgb(
            egui::lerp(off.r() as f32..=ACCENT.r() as f32, t) as u8,
            egui::lerp(off.g() as f32..=ACCENT.g() as f32, t) as u8,
            egui::lerp(off.b() as f32..=ACCENT.b() as f32, t) as u8,
        );
        let radius = rect.height() / 2.0;
        ui.painter().rect_filled(rect, radius, fill);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        ui.painter().circle_filled(egui::pos2(x, rect.center().y), radius * 0.78, Color32::WHITE);
    }
    response
}

/// A label with a switch on the right. Returns true when flipped.
pub fn toggle_row(ui: &mut egui::Ui, on: &mut bool, label: &str, hint: Option<&str>) -> bool {
    let mut changed = false;
    let height = if hint.is_some() { 40.0 } else { 28.0 };
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            match hint {
                Some(h) => {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(label);
                        ui.label(RichText::new(h).small().color(weak(ui)));
                    });
                }
                None => {
                    ui.label(label);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                changed = toggle(ui, on).changed();
            });
        },
    );
    changed
}

/// A colored dot and text in a soft pill (drawn directly, so it's only as wide as its text).
pub fn pill(ui: &mut egui::Ui, color: Color32, text: &str) {
    let font = FontId::new(12.5, FontFamily::Name("bold".into()));
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let (pad, dot, gap) = (Vec2::new(11.0, 6.0), 8.0, 7.0);
    let size = Vec2::new(pad.x * 2.0 + dot + gap + galley.size().x, pad.y * 2.0 + galley.size().y.max(dot));
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, size.y / 2.0, color.linear_multiply(0.16));
    p.circle_filled(egui::pos2(rect.left() + pad.x + dot / 2.0, rect.center().y), dot / 2.0, color);
    let text_pos = egui::pos2(rect.left() + pad.x + dot + gap, rect.center().y - galley.size().y / 2.0);
    p.galley(text_pos, galley, color);
}
