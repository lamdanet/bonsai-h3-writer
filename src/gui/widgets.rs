//! Blender-like building blocks shared by every workspace.

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, Response, RichText, Sense, Stroke, Ui};

use super::theme::{self, Palette};

/// Icon glyphs available in egui's bundled fonts.
pub mod icons {
    pub const WRITE: &str = "✏";
    pub const CHAT: &str = "💬";
    pub const LIBRARY: &str = "📚";
    pub const HISTORY: &str = "🕘";
    pub const SERVER: &str = "🖥";
    pub const SETTINGS: &str = "⚙";
    pub const PLAY: &str = "▶";
    pub const STOP: &str = "⏹";
    pub const RESTART: &str = "🔄";
    pub const FOLDER: &str = "📁";
    pub const COPY: &str = "📋";
    pub const SAVE: &str = "💾";
    pub const TRASH: &str = "🗑";
    pub const ADD: &str = "➕";
    pub const REMOVE: &str = "➖";
    pub const CHECK: &str = "✔";
    pub const CROSS: &str = "❌";
    pub const WARN: &str = "⚠";
    pub const INFO: &str = "ℹ";
    pub const STAR: &str = "★";
    pub const POWER: &str = "🔌";
    pub const IMAGE: &str = "🖼";
    pub const THINK: &str = "💡";
    pub const SEARCH: &str = "🔍";
    pub const NOTE: &str = "📝";
    pub const UP: &str = "⏶";
    pub const DOWN: &str = "⏷";
}

/// Colour for a lint severity / toast kind name.
pub fn severity_color(sev: bh3::lint::Severity) -> Color32 {
    match sev {
        bh3::lint::Severity::Error => Palette::ERR,
        bh3::lint::Severity::Warning => Palette::WARN,
        bh3::lint::Severity::Info => Palette::INFO,
    }
}

/// Blender editor header strip: title on the left, `add_right` laid out right-to-left.
pub fn area_header(ui: &mut Ui, title: &str, add_right: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(Palette::HEADER).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong().color(Palette::TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), add_right);
        });
    });
}

/// Collapsible Blender panel (darker header bar with a disclosure triangle, lighter body).
/// The open state is remembered per `id_salt`. Returns whether it is open.
pub fn panel(ui: &mut Ui, id_salt: &str, title: &str, default_open: bool, add: impl FnOnce(&mut Ui)) -> bool {
    let id = ui.make_persistent_id(("bh3-panel", id_salt));
    let mut open = ui.data_mut(|d| *d.get_persisted_mut_or(id, default_open));
    let header_h = 24.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), header_h), Sense::click());
    let radius = if open { CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 } } else { CornerRadius::same(5) };
    let fill = if resp.hovered() { Color32::from_gray(66) } else { Palette::PANEL_HEADER };
    ui.painter().rect_filled(rect, radius, fill);
    let arrow = if open { "⏷" } else { "⏵" };
    ui.painter().text(
        rect.left_center() + egui::vec2(8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        format!("{arrow}  {title}"),
        egui::FontId::proportional(13.5),
        Palette::TEXT,
    );
    // Blender's drag grip on the right of panel headers: two columns of dots.
    for col in 0..2 {
        for row in 0..3 {
            let dot = egui::pos2(rect.right() - 16.0 + col as f32 * 4.0, rect.center().y - 4.0 + row as f32 * 4.0);
            ui.painter().circle_filled(dot, 1.0, Palette::TEXT_FAINT);
        }
    }
    if resp.clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_persisted(id, open));
    }
    if open {
        // Header and body form one box (no item spacing between them).
        ui.add_space(-ui.spacing().item_spacing.y);
        egui::Frame::new()
            .fill(Palette::PANEL_BODY)
            .corner_radius(CornerRadius { nw: 0, ne: 0, sw: 5, se: 5 })
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui);
            });
    }
    ui.add_space(6.0);
    open
}

/// Property row: dim right-aligned label in the left ~40 %, widget(s) in the rest.
pub fn prop_row<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let label_w = (ui.available_width() * 0.4).clamp(70.0, 170.0);
        ui.allocate_ui_with_layout(egui::vec2(label_w, 22.0), Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(label).color(Palette::TEXT_DIM));
        });
        add(ui)
    })
    .inner
}

/// Blender "expanded enum": joined buttons, the current value highlighted blue. Returns true on change.
pub fn enum_buttons<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 1.0;
        let n = options.len().max(1) as f32;
        let w = ((ui.available_width() - (n - 1.0)) / n).max(30.0);
        for (opt, label) in options {
            let selected = *value == *opt;
            let button =
                egui::Button::new(RichText::new(*label).color(if selected { Color32::WHITE } else { Palette::TEXT }))
                    .fill(if selected { Palette::BLUE } else { Palette::WIDGET })
                    .min_size(egui::vec2(w, 22.0));
            if ui.add(button).clicked() && !selected {
                *value = *opt;
                changed = true;
            }
        }
    });
    changed
}

/// Big accent-coloured main action button (like Blender's Render button).
pub fn primary_button(ui: &mut Ui, text: &str, enabled: bool) -> Response {
    let accent = theme::accent(ui.ctx());
    let fill = if enabled { accent } else { Palette::WIDGET };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(Color32::WHITE).size(14.5))
            .fill(fill)
            .min_size(egui::vec2(ui.available_width(), 30.0)),
    )
}

/// Small flat icon button with a tooltip.
pub fn icon_button(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    ui.add(egui::Button::new(icon).frame(true).min_size(egui::vec2(24.0, 22.0))).on_hover_text(tooltip)
}

/// Coloured dot followed by text.
pub fn status_dot(ui: &mut Ui, color: Color32, text: &str) -> Response {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.5, color);
        ui.label(text)
    })
    .inner
}

/// Rounded coloured label. The response covers the whole badge and senses clicks (tooltips,
/// clickable badges).
pub fn badge(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let shown = egui::Frame::new()
        .fill(color.gamma_multiply(0.25))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.7)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| ui.add(egui::Label::new(RichText::new(text).small().color(color)).selectable(false)));
    shown.response.interact(Sense::click())
}

/// Small dim upper-case section label.
pub fn section_label(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text.to_uppercase()).small().color(Palette::TEXT_DIM));
}

/// "Copy" button that puts `text` on the clipboard. Returns true when clicked.
pub fn copy_button(ui: &mut Ui, text: &str, label: &str) -> bool {
    let clicked = ui
        .add_enabled(!text.is_empty(), egui::Button::new(format!("{} {label}", icons::COPY)))
        .on_hover_text("Copy to the clipboard")
        .clicked();
    if clicked {
        ui.ctx().copy_text(text.to_owned());
    }
    clicked
}

/// Monospace text area on the dark input background. Read-only when `editable` is false.
pub fn code_view(ui: &mut Ui, id_salt: &str, text: &mut String, rows: usize, editable: bool) -> Response {
    let edit = egui::TextEdit::multiline(text)
        .id_salt(id_salt)
        .font(egui::TextStyle::Monospace)
        .desired_rows(rows)
        .desired_width(f32::INFINITY)
        .lock_focus(true)
        .interactive(editable)
        .background_color(Palette::INPUT);
    ui.add(edit)
}

/// Horizontal separator line in the outline colour.
pub fn thin_separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, Palette::OUTLINE);
}

/// Format a byte/MB amount as "6.2 GB" / "830 MB".
pub fn fmt_mb(mb: u32) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f32 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

/// Format seconds as "4m 05s" / "52s".
pub fn fmt_secs(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// PNG bytes of the app icon (`assets/icon.png`).
pub const APP_ICON_PNG: &[u8] = include_bytes!("../../assets/icon.png");

/// The app icon, `size` points square. Mipmapped, so it stays smooth when drawn small.
pub fn app_icon(size: f32) -> egui::Image<'static> {
    egui::Image::from_bytes("bytes://bh3-icon.png", APP_ICON_PNG)
        .texture_options(egui::TextureOptions::LINEAR.with_mipmap_mode(Some(egui::TextureFilter::Linear)))
        .fit_to_exact_size(egui::vec2(size, size))
}

/// `BH3_DEMO=1`: workspaces may show sample content for screenshots. Always false in normal runs.
pub fn demo_mode() -> bool {
    static DEMO: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DEMO.get_or_init(|| std::env::var("BH3_DEMO").is_ok_and(|v| v.trim() == "1"))
}

/// Frame of modal dialogs: dark popup background, thin outline, soft shadow, 16 px margin.
pub fn dialog_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(Palette::DIALOG)
        .stroke(Stroke::new(1.0_f32, Color32::from_gray(16)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(16))
        .shadow(egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: Color32::from_black_alpha(120) })
}

/// Blender menu entry: icon column, label, shortcut right-aligned in dim text, blue highlight on
/// hover. Disabled entries are greyed out but still show their tooltip. Closes the menu on click.
pub fn menu_item(ui: &mut Ui, icon: &str, label: &str, shortcut: &str, enabled: bool) -> Response {
    const ICON_COL: f32 = 26.0;
    const HEIGHT: f32 = 22.0;
    let text_color = if enabled { Palette::TEXT } else { Palette::TEXT_FAINT };
    let label_galley =
        ui.painter().layout_no_wrap(label.to_owned(), egui::FontId::proportional(13.5), Color32::PLACEHOLDER);
    let shortcut_galley = (!shortcut.is_empty()).then(|| {
        ui.painter().layout_no_wrap(shortcut.to_owned(), egui::FontId::proportional(12.5), Color32::PLACEHOLDER)
    });
    let needed = ICON_COL + label_galley.size().x + shortcut_galley.as_ref().map_or(0.0, |g| 32.0 + g.size().x) + 10.0;
    let width = ui.available_width().max(needed);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, HEIGHT), sense);
    if ui.is_rect_visible(rect) {
        let hot = enabled && response.hovered();
        let painter = ui.painter();
        if hot {
            painter.rect_filled(rect, CornerRadius::same(3), Palette::BLUE);
        }
        let fg = if hot { Color32::WHITE } else { text_color };
        if !icon.is_empty() {
            let icon_color = if hot {
                Color32::WHITE
            } else if enabled {
                Palette::TEXT_DIM
            } else {
                Palette::TEXT_FAINT
            };
            painter.text(
                egui::pos2(rect.left() + ICON_COL / 2.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                icon,
                egui::FontId::proportional(13.0),
                icon_color,
            );
        }
        let label_pos = egui::pos2(rect.left() + ICON_COL, rect.center().y - label_galley.size().y / 2.0);
        painter.galley_with_override_text_color(label_pos, label_galley, fg);
        if let Some(galley) = shortcut_galley {
            let pos = egui::pos2(rect.right() - 8.0 - galley.size().x, rect.center().y - galley.size().y / 2.0);
            let color = if hot { Color32::from_gray(225) } else { Palette::TEXT_FAINT };
            painter.galley_with_override_text_color(pos, galley, color);
        }
    }
    if response.clicked() {
        ui.close_menu();
    }
    response
}

/// Thin line between groups of menu entries.
pub fn menu_separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 7.0), Sense::hover());
    ui.painter().hline(
        rect.left() + 4.0..=rect.right() - 4.0,
        rect.center().y,
        Stroke::new(1.0_f32, Color32::from_gray(62)),
    );
}

/// Colour for a usage fraction (VRAM, context): green, yellow from 80 %, red from 97 %.
pub fn usage_color(fraction: f32) -> Color32 {
    if fraction >= 0.97 {
        Palette::ERR
    } else if fraction >= 0.8 {
        Palette::WARN
    } else {
        Palette::OK
    }
}

/// Model file name for tight spaces: no folder, no `.gguf`, and a long name is cut in the middle
/// while the size and quantization stay readable:
/// `Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf` -> `Ternary-Bonsai-2-27B…PTQ1_0`.
pub fn short_model_name(name: &str, max_chars: usize) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = if file.to_ascii_lowercase().ends_with(".gguf") { &file[..file.len() - 5] } else { file };
    if stem.chars().count() <= max_chars {
        return stem.to_string();
    }
    let tokens: Vec<&str> = stem.split('-').collect();
    let is_size = |t: &&str| t.strip_suffix(['B', 'b']).is_some_and(|n| !n.is_empty() && n.parse::<f32>().is_ok());
    if let (Some(size_at), Some(quant)) = (tokens.iter().position(is_size), tokens.last()) {
        if size_at + 1 < tokens.len() - 1 {
            let short = format!("{}…{quant}", tokens[..=size_at].join("-"));
            if short.chars().count() <= max_chars {
                return short;
            }
        }
    }
    let chars: Vec<char> = stem.chars().collect();
    let keep = max_chars.max(4) - 1;
    let head = keep * 2 / 3;
    let tail = keep - head;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use super::{icons, short_model_name};

    /// Every icon glyph of the shell must exist in the fonts the app loads (a missing one renders
    /// as an empty box).
    #[test]
    fn icon_glyphs_exist_in_the_fonts() {
        let ctx = eframe::egui::Context::default();
        super::theme::install_fonts(&ctx);
        let _ = ctx.run(Default::default(), |_| {});
        let used = [
            icons::WRITE,
            icons::CHAT,
            icons::LIBRARY,
            icons::HISTORY,
            icons::SERVER,
            icons::SETTINGS,
            icons::PLAY,
            icons::STOP,
            icons::RESTART,
            icons::FOLDER,
            icons::ADD,
            icons::CHECK,
            icons::WARN,
            icons::INFO,
            icons::NOTE,
            icons::SEARCH,
            icons::LIBRARY,
            icons::TRASH,
            "⬇",
            "⏬",
            "⏸",
            "🚪",
            "⏏",
            "🌐",
            "🎨",
            "✖",
            "…",
            "›",
            "°",
            "·",
            "•",
            "⏷",
            "⏵",
        ];
        let font = eframe::egui::FontId::proportional(13.5);
        let missing: Vec<&str> = used.into_iter().filter(|g| !ctx.fonts(|f| f.has_glyphs(&font, g))).collect();
        assert!(missing.is_empty(), "glyphs missing from the fonts: {missing:?}");
    }

    #[test]
    fn short_model_names() {
        assert_eq!(short_model_name("Bonsai2-27B-PTQ1_0.gguf", 28), "Bonsai2-27B-PTQ1_0");
        assert_eq!(
            short_model_name(r"D:\m\Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf", 28),
            "Ternary-Bonsai-2-27B…PTQ1_0"
        );
        let generic = short_model_name("averyveryverylongmodelnamewithoutanydashes.gguf", 20);
        assert_eq!(generic.chars().count(), 20);
        assert!(generic.contains('…'));
    }
}
