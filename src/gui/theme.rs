//! Blender-style dark theme: grey areas separated by thin dark gaps, flat rounded widgets,
//! blue for active widgets, orange accent for the active workspace and the main action.

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle};

/// Blender 4.x-like colours.
pub struct Palette;

impl Palette {
    /// Gaps between areas / window background.
    pub const GAP: Color32 = Color32::from_rgb(22, 22, 22);
    /// Top bar and status bar.
    pub const BAR: Color32 = Color32::from_rgb(35, 35, 35);
    /// Editor headers (area header strips).
    pub const HEADER: Color32 = Color32::from_rgb(48, 48, 48);
    /// Main editor region.
    pub const REGION: Color32 = Color32::from_rgb(43, 43, 43);
    /// Side regions (toolbar, N-panel).
    pub const SIDE: Color32 = Color32::from_rgb(38, 38, 38);
    /// Panel header strip.
    pub const PANEL_HEADER: Color32 = Color32::from_rgb(58, 58, 58);
    /// Panel body.
    pub const PANEL_BODY: Color32 = Color32::from_rgb(50, 50, 50);
    /// Buttons, dropdowns.
    pub const WIDGET: Color32 = Color32::from_rgb(84, 84, 84);
    pub const WIDGET_HOVER: Color32 = Color32::from_rgb(101, 101, 101);
    /// Text fields, code views.
    pub const INPUT: Color32 = Color32::from_rgb(29, 29, 29);
    /// Active / selected widgets (Blender blue).
    pub const BLUE: Color32 = Color32::from_rgb(71, 114, 179);
    pub const BLUE_HOVER: Color32 = Color32::from_rgb(84, 132, 204);
    /// Default accent (Blender orange) - the configured accent overrides it at runtime.
    pub const ORANGE: Color32 = Color32::from_rgb(232, 125, 13);
    pub const TEXT: Color32 = Color32::from_rgb(230, 230, 230);
    pub const TEXT_DIM: Color32 = Color32::from_rgb(150, 150, 150);
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(105, 105, 105);
    pub const OUTLINE: Color32 = Color32::from_rgb(24, 24, 24);
    pub const OK: Color32 = Color32::from_rgb(110, 190, 90);
    pub const WARN: Color32 = Color32::from_rgb(230, 180, 60);
    pub const ERR: Color32 = Color32::from_rgb(230, 85, 70);
    pub const INFO: Color32 = Color32::from_rgb(110, 160, 230);
    /// Modal dialogs (setup, About, confirmations).
    pub const DIALOG: Color32 = Color32::from_rgb(40, 40, 40);
    /// Hover fill of flat items on the bars (menu titles, workspace tabs, status segments).
    pub const BAR_HOVER: Color32 = Color32::from_rgb(52, 52, 52);
}

/// Accent presets offered in Settings: (name, colour).
pub const ACCENT_PRESETS: [(&str, [u8; 3]); 4] = [
    ("Blender orange", [232, 125, 13]),
    ("Blue", [66, 140, 230]),
    ("Green", [92, 170, 76]),
    ("Purple", [156, 104, 224]),
];

/// The configured accent colour (stored in egui memory by [`apply`]).
pub fn accent(ctx: &egui::Context) -> Color32 {
    ctx.data(|d| d.get_temp::<Color32>(egui::Id::new("bh3-accent"))).unwrap_or(Palette::ORANGE)
}

/// Monospace font of `size` points.
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Load Segoe UI (Windows) as the main font when available - closer to Blender's clean UI font
/// than egui's default. Call once at start-up.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let candidates = [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\Inter-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("ui".to_owned(), std::sync::Arc::new(FontData::from_owned(bytes)));
            if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
                family.insert(0, "ui".to_owned());
            }
            break;
        }
    }
    for path in [r"C:\Windows\Fonts\consola.ttf", r"C:\Windows\Fonts\CascadiaMono.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("mono".to_owned(), std::sync::Arc::new(FontData::from_owned(bytes)));
            if let Some(family) = fonts.families.get_mut(&FontFamily::Monospace) {
                family.insert(0, "mono".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

/// Apply the theme. Cheap enough to call when settings change (not needed every frame).
///
/// egui follows the Windows light/dark setting by default and keeps one style per theme, so the
/// theme is pinned to dark and both slots get the app's style: Windows Light mode or High Contrast
/// never swaps in egui's light style under the dark palette.
pub fn apply(ctx: &egui::Context, accent_rgb: [u8; 3], scale: f32) {
    let accent = Color32::from_rgb(accent_rgb[0], accent_rgb[1], accent_rgb[2]);
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("bh3-accent"), accent));
    ctx.set_zoom_factor(scale.clamp(0.6, 2.5));
    ctx.set_theme(egui::Theme::Dark);

    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.text_styles = [
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    style.spacing.interact_size = egui::vec2(36.0, 22.0);
    style.spacing.combo_height = 320.0;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    style.spacing.slider_width = 160.0;
    // Compact menus like Blender's (egui's default starts menus 400 pt wide).
    style.spacing.menu_width = 260.0;

    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = Palette::REGION;
    v.window_fill = Palette::HEADER;
    v.window_stroke = Stroke::new(1.0_f32, Palette::OUTLINE);
    v.window_corner_radius = CornerRadius::same(6);
    v.menu_corner_radius = CornerRadius::same(4);
    v.extreme_bg_color = Palette::INPUT;
    v.faint_bg_color = Palette::PANEL_BODY;
    v.code_bg_color = Palette::INPUT;
    v.hyperlink_color = Color32::from_rgb(108, 155, 224);
    v.selection.bg_fill = Palette::BLUE;
    v.selection.stroke = Stroke::new(1.0_f32, Palette::TEXT);
    v.warn_fg_color = Palette::WARN;
    v.error_fg_color = Palette::ERR;
    v.striped = true;
    v.indent_has_left_vline = false;
    // Blender-like sliders: the part left of the handle is filled with the active blue.
    v.slider_trailing_fill = true;

    let radius = CornerRadius::same(4);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = Palette::REGION;
    w.noninteractive.weak_bg_fill = Palette::REGION;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, Palette::OUTLINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, Palette::TEXT);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = Palette::WIDGET;
    w.inactive.weak_bg_fill = Palette::WIDGET;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, Palette::OUTLINE);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, Palette::TEXT);
    w.inactive.corner_radius = radius;

    w.hovered.bg_fill = Palette::WIDGET_HOVER;
    w.hovered.weak_bg_fill = Palette::WIDGET_HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_gray(120));
    w.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = Palette::BLUE;
    w.active.weak_bg_fill = Palette::BLUE;
    w.active.bg_stroke = Stroke::new(1.0_f32, Palette::BLUE_HOVER);
    w.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;

    w.open.bg_fill = Palette::BLUE;
    w.open.weak_bg_fill = Palette::BLUE;
    w.open.bg_stroke = Stroke::new(1.0_f32, Palette::BLUE_HOVER);
    w.open.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    w.open.corner_radius = radius;

    let style = std::sync::Arc::new(style);
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_style_whatever_the_system_theme() {
        let ctx = egui::Context::default();
        apply(&ctx, [232, 125, 13], 1.0);
        // eframe reports the Windows setting in every pass (Light also for High Contrast).
        for system in [egui::Theme::Light, egui::Theme::Dark, egui::Theme::Light] {
            let input = egui::RawInput { system_theme: Some(system), ..Default::default() };
            let _ = ctx.run(input, |_| {});
            let style = ctx.style();
            assert_eq!(ctx.theme(), egui::Theme::Dark);
            assert!(style.visuals.dark_mode);
            assert_eq!(style.visuals.panel_fill, Palette::REGION);
            assert_eq!(style.text_styles[&TextStyle::Body].size, 13.5);
        }
        assert_eq!(ctx.style_of(egui::Theme::Light).visuals.panel_fill, Palette::REGION);
    }
}
