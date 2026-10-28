//! Settings workspace, laid out like Blender's Preferences: categories on the left, the selected
//! category's panels of property rows in the middle. Every change applies and is saved at once
//! (a dragged slider or colour when the mouse button is released). Drawing a page never changes a
//! value: widgets clamp only what the user edits, so values set in Write, Chat or config.json stay.

use std::ops::RangeInclusive;
use std::path::PathBuf;

use bh3::config::{AppConfig, EnvVar};
use bh3::gen::{GenParams, ThinkingMode};
use bh3::lengths;
use bh3::library::LibraryItem;
use eframe::egui::{
    self, emath::Numeric, Align, Align2, Color32, CornerRadius, FontId, Id, Key, Layout, Margin, RichText, Sense,
    Stroke, StrokeKind, Ui,
};

use super::app::{AppState, ToastKind, Workspace};
use super::theme::{self, Palette};
use super::topbar;
use super::util;
use super::widgets::{self, icons};

/// Widest the panels get.
const MAX_WIDTH: f32 = 760.0;
/// Width of the value box of sliders, so their right edges line up.
const VALUE_W: f32 = 58.0;
/// Size of hints under property rows.
const HINT_SIZE: f32 = 12.0;
/// Re-read folder sizes and counts of the Data page this often (seconds).
const DATA_REFRESH_SECS: f64 = 3.0;
/// Range of the VRAM busy limit (MB), the same as the Bonsai workspace's field.
const VRAM_BUSY_RANGE: RangeInclusive<u32> = 0..=16_384;

/// Value ranges of a generation-settings editor (top-p and min-p are always 0..=1).
pub struct ParamRanges {
    pub temperature: RangeInclusive<f32>,
    pub max_tokens: RangeInclusive<u32>,
    pub top_k: RangeInclusive<i32>,
}

/// Ranges of `config.writer_params`, as in the Write workspace's Generation panel.
pub const WRITER_RANGES: ParamRanges = ParamRanges { temperature: 0.0..=1.5, max_tokens: 64..=16_384, top_k: 0..=200 };
/// Ranges of `config.chat_params`, as in the Chat workspace's Generation panel ("Use as default").
pub const CHAT_RANGES: ParamRanges = ParamRanges { temperature: 0.0..=2.0, max_tokens: 16..=32_768, top_k: 0..=500 };

/// Categories of the left column.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Category {
    #[default]
    Interface,
    Server,
    Writing,
    Chat,
    Data,
    About,
}

impl Category {
    /// Groups of the left column (a gap between groups, as in Blender's Preferences).
    const GROUPS: [&'static [Category]; 3] = [
        &[Category::Interface],
        &[Category::Server, Category::Writing, Category::Chat],
        &[Category::Data, Category::About],
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Interface => "Interface",
            Category::Server => "Bonsai server",
            Category::Writing => "Prompt writing",
            Category::Chat => "Chat",
            Category::Data => "Data",
            Category::About => "About",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Category::Interface => "🎨",
            Category::Server => icons::SERVER,
            Category::Writing => icons::WRITE,
            Category::Chat => icons::CHAT,
            Category::Data => icons::FOLDER,
            Category::About => icons::INFO,
        }
    }

    /// From a name like `interface`, `server` / `bonsai`, `writing` / `prompt`, `chat`, `data`, `about`
    /// (`BH3_SETTINGS_CATEGORY`, for screenshots).
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "interface" | "ui" => Some(Category::Interface),
            "server" | "bonsai" => Some(Category::Server),
            "writing" | "prompt" | "prompts" | "writer" => Some(Category::Writing),
            "chat" => Some(Category::Chat),
            "data" => Some(Category::Data),
            "about" => Some(Category::About),
            _ => None,
        }
    }
}

/// Folders, counts and sizes shown on the Data page (re-read every few seconds, not every frame).
struct DataInfo {
    config_dir: PathBuf,
    history_dir: PathBuf,
    history_count: usize,
    library_dir: PathBuf,
    log_path: PathBuf,
    log_size: Option<u64>,
    read_at: f64,
}

impl DataInfo {
    fn read(now: f64) -> Self {
        let history_dir = bh3::history::history_dir();
        let history_count = std::fs::read_dir(&history_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
                    .count()
            })
            .unwrap_or(0);
        let log_path = bh3::server::default_log_path();
        let log_size = std::fs::metadata(&log_path).ok().filter(|m| m.is_file()).map(|m| m.len());
        Self {
            config_dir: bh3::config::config_dir(),
            history_dir,
            history_count,
            library_dir: bh3::library::library_dir(),
            log_path,
            log_size,
            read_at: now,
        }
    }
}

/// State of the Settings workspace.
#[derive(Default)]
pub struct SettingsState {
    /// The category shown (the top bar's AUTO badge opens Bonsai server).
    pub category: Category,
    /// `BH3_SETTINGS_CATEGORY` was read.
    env_checked: bool,
    /// UI scale while its slider is dragged or a percentage is typed (applied on release, Enter or
    /// focus loss, so the interface does not jump while "125" is typed).
    scale_draft: Option<f32>,
    /// Server URL being edited (applied on Enter, focus loss or leaving the page; Esc drops it).
    url_draft: Option<String>,
    /// A drag changed the config: save once the mouse button is released.
    pending_save: bool,
    confirm_reset: bool,
    data: Option<DataInfo>,
}

/// Save settings changed by a drag once the mouse button is up, and apply the drafts once the
/// Settings workspace was left (a hidden field loses its focus without noticing). Called every
/// frame by the top bar, so it also happens after leaving the Settings workspace.
pub fn flush_pending(app: &mut AppState, ctx: &egui::Context) {
    let has_draft = app.settings.url_draft.is_some() || app.settings.scale_draft.is_some();
    if has_draft && app.workspace != Workspace::Settings {
        let mut st = std::mem::take(&mut app.settings);
        commit_hidden_drafts(&mut st, app, None);
        st.pending_save = true;
        app.settings = st;
    }
    if app.settings.pending_save && !ctx.input(|i| i.pointer.any_down()) {
        app.settings.pending_save = false;
        app.save_config();
    }
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    let mut st = std::mem::take(&mut app.settings);
    if !st.env_checked {
        st.env_checked = true;
        if let Some(category) = std::env::var("BH3_SETTINGS_CATEGORY").ok().and_then(|n| Category::from_name(&n)) {
            st.category = category;
        }
    }
    let before = app.config.clone();

    nav(ctx, &mut st);
    egui::CentralPanel::default().frame(egui::Frame::new().fill(Palette::REGION)).show(ctx, |ui| {
        widgets::area_header(ui, &format!("Settings  ›  {}", st.category.label()), |_| {});
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 24 }).show(ui, |ui| {
                ui.set_max_width(MAX_WIDTH);
                match st.category {
                    Category::Interface => interface_page(ui, &mut st, app),
                    Category::Server => server_page(ui, &mut st, app),
                    Category::Writing => writing_page(ui, app),
                    Category::Chat => chat_page(ui, app),
                    Category::Data => data_page(ui, &mut st, app),
                    Category::About => about_page(ui),
                }
            });
        });
    });
    reset_dialog(ctx, &mut st, app);
    let shown = st.category;
    commit_hidden_drafts(&mut st, app, Some(shown));

    if app.config != before {
        if ctx.input(|i| i.pointer.any_down()) {
            st.pending_save = true;
        } else {
            app.save_config();
        }
    }
    app.settings = st;
}

/// Apply the drafts of the pages other than `shown` (`None`: Settings is not drawn). egui drops
/// the focus of a field that is not drawn, so its "lost focus" would never be seen.
fn commit_hidden_drafts(st: &mut SettingsState, app: &mut AppState, shown: Option<Category>) {
    if shown != Some(Category::Server) {
        commit_url_draft(st, app, false);
    }
    if shown != Some(Category::Interface) {
        commit_scale_draft(st, app, false);
    }
}

/// Apply the UI-scale draft, rounded to 5 % (`escaped`: drop it).
fn commit_scale_draft(st: &mut SettingsState, app: &mut AppState, escaped: bool) {
    if let Some(value) = st.scale_draft.take().filter(|_| !escaped) {
        app.config.ui_scale = ((value * 20.0).round() / 20.0).clamp(topbar::SCALE_MIN, topbar::SCALE_MAX);
    }
}

/// Apply the server-URL draft (`escaped`: drop it) and toast the outcome.
fn commit_url_draft(st: &mut SettingsState, app: &mut AppState, escaped: bool) {
    if let Some(draft) = st.url_draft.take() {
        if let Some((kind, text)) = apply_url_draft(&mut app.config, &draft, escaped) {
            app.toast(kind, text);
        }
    }
}

/// Store an edited server URL in `config` unless it was dropped, unchanged or invalid; returns the
/// toast to show.
fn apply_url_draft(config: &mut AppConfig, draft: &str, escaped: bool) -> Option<(ToastKind, &'static str)> {
    let url = draft.trim().trim_end_matches('/');
    if escaped || url == config.server_url {
        // Undo, or nothing changed.
        None
    } else if valid_url(url) {
        config.server_url = url.to_string();
        Some((ToastKind::Ok, "Server address saved"))
    } else {
        Some((ToastKind::Warn, "Not saved: the server address must start with http:// or https://"))
    }
}

// --- left column -----------------------------------------------------------------------------

fn nav(ctx: &egui::Context, st: &mut SettingsState) {
    egui::SidePanel::left("bh3-settings-nav")
        .resizable(false)
        .exact_width(186.0)
        .frame(egui::Frame::new().fill(Palette::SIDE).inner_margin(Margin { left: 8, right: 8, top: 10, bottom: 8 }))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (index, group) in Category::GROUPS.iter().enumerate() {
                if index > 0 {
                    ui.add_space(12.0);
                }
                for &category in *group {
                    if nav_item(ui, category, st.category == category).clicked() {
                        st.category = category;
                    }
                }
            }
            ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                let text = format!("{} Saved automatically", icons::CHECK);
                ui.label(RichText::new(text).size(HINT_SIZE).color(Palette::TEXT_FAINT))
                    .on_hover_text("Every change is saved to config.json at once (File > Open config folder)");
            });
        });
}

fn nav_item(ui: &mut Ui, category: Category, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 24.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let hovered = response.hovered();
        if selected {
            painter.rect_filled(rect, CornerRadius::same(4), Palette::BLUE);
        } else if hovered {
            painter.rect_filled(rect, CornerRadius::same(4), Color32::from_gray(52));
        }
        let fg = if selected {
            Color32::WHITE
        } else if hovered {
            Palette::TEXT
        } else {
            Color32::from_gray(205)
        };
        let icon_fg = if selected { Color32::WHITE } else { Palette::TEXT_DIM };
        let y = rect.center().y;
        painter.text(
            egui::pos2(rect.left() + 14.0, y),
            Align2::CENTER_CENTER,
            category.icon(),
            FontId::proportional(13.0),
            icon_fg,
        );
        painter.text(
            egui::pos2(rect.left() + 30.0, y),
            Align2::LEFT_CENTER,
            category.label(),
            FontId::proportional(13.5),
            fg,
        );
    }
    response
}

// --- shared building blocks ------------------------------------------------------------------

/// Width of the label column of [`widgets::prop_row`] plus the gap after it.
fn label_column(ui: &Ui) -> f32 {
    (ui.available_width() * 0.4).clamp(70.0, 170.0) + ui.spacing().item_spacing.x
}

/// Dim wrapped hint aligned with the widget column of the property rows.
fn hint(ui: &mut Ui, text: &str) {
    hint_colored(ui, text, Palette::TEXT_DIM);
}

fn hint_colored(ui: &mut Ui, text: &str, color: Color32) {
    ui.horizontal(|ui| {
        ui.add_space(label_column(ui));
        ui.add(egui::Label::new(RichText::new(text).size(HINT_SIZE).color(color)).wrap());
    });
    ui.add_space(2.0);
}

/// Make the next slider fill the row with a fixed-width value box.
fn fit_slider(ui: &mut Ui, reserve: f32) {
    let spacing = ui.spacing().item_spacing.x;
    ui.spacing_mut().interact_size.x = VALUE_W;
    ui.spacing_mut().slider_width = (ui.available_width() - VALUE_W - spacing - reserve).max(60.0);
}

fn slider_row<N: Numeric>(
    ui: &mut Ui,
    label: &str,
    value: &mut N,
    range: RangeInclusive<N>,
    decimals: usize,
    tip: &str,
) {
    widgets::prop_row(ui, label, |ui| {
        fit_slider(ui, 0.0);
        let slider = egui::Slider::new(value, range).fixed_decimals(decimals).clamping(egui::SliderClamping::Edits);
        ui.add(slider).on_hover_text(tip);
    });
}

/// Width a plain button with `text` takes (at least `min`).
fn button_width(ui: &Ui, text: &str, min: f32) -> f32 {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(13.5), Color32::PLACEHOLDER);
    (galley.size().x + 2.0 * ui.spacing().button_padding.x).max(min).ceil()
}

/// Full-width number field (Blender style).
fn number_field(ui: &mut Ui, drag: egui::DragValue<'_>, tip: &str) -> egui::Response {
    ui.add_sized([ui.available_width(), 22.0], drag).on_hover_text(tip)
}

/// Read-only path in a text-field look, cut with "…" when too long; hover shows it in full.
fn path_field(ui: &mut Ui, text: &str, width: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width.max(40.0), 22.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(4),
            Palette::INPUT,
            Stroke::new(1.0_f32, Palette::OUTLINE),
            StrokeKind::Inside,
        );
        let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), theme::mono(12.5), Palette::TEXT);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 14.0);
        let galley = painter.layout_job(job);
        painter.galley(egui::pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0), galley, Palette::TEXT);
    }
    response.on_hover_text(text)
}

// --- Interface -------------------------------------------------------------------------------

fn interface_page(ui: &mut Ui, st: &mut SettingsState, app: &mut AppState) {
    widgets::panel(ui, "settings-display", "Display", true, |ui| {
        widgets::prop_row(ui, "Resolution scale", |ui| scale_slider(ui, st, app));
        hint(ui, "Size of the whole interface. Also View > Zoom: Ctrl+Plus, Ctrl+Minus, Ctrl+0.");
        widgets::prop_row(ui, "Accent colour", |ui| accent_picker(ui, app));
        hint(ui, "Marks the active workspace tab and the main buttons (Write prompt, Use this folder).");
    });
    widgets::panel(ui, "settings-text", "Text", true, |ui| {
        widgets::prop_row(ui, "Prompts", |ui| {
            ui.checkbox(&mut app.config.monospace_prompts, "Monospace font")
                .on_hover_text("Show prompts in a fixed-width font: tags, spacing and timestamps are easier to check");
        });
        widgets::prop_row(ui, "Reasoning", |ui| {
            ui.checkbox(&mut app.config.show_reasoning, "Show Bonsai's reasoning")
                .on_hover_text("With thinking on, show the reasoning text (collapsible) above the answer");
        });
    });
}

fn scale_slider(ui: &mut Ui, st: &mut SettingsState, app: &mut AppState) {
    let reset_w = button_width(ui, icons::RESTART, 26.0);
    fit_slider(ui, reset_w + ui.spacing().item_spacing.x);
    let mut value = st.scale_draft.unwrap_or(app.config.ui_scale);
    let slider = egui::Slider::new(&mut value, topbar::SCALE_MIN..=topbar::SCALE_MAX)
        .clamping(egui::SliderClamping::Edits)
        .step_by(0.05)
        .custom_formatter(|v, _| format!("{:.0} %", v * 100.0))
        .custom_parser(|s| s.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|p| p / 100.0));
    let response = ui.add(slider).on_hover_text("Drag, or double-click the number to type a percentage");
    // The response has the value box's focus while a number is typed into it.
    if response.dragged() || response.has_focus() {
        st.scale_draft = Some(value);
    } else if response.lost_focus() {
        // Enter or a click elsewhere applies the typed number, Esc drops it.
        st.scale_draft = Some(value);
        commit_scale_draft(st, app, ui.input(|i| i.key_pressed(Key::Escape)));
    } else if response.changed() || response.drag_stopped() {
        st.scale_draft = Some(value);
        commit_scale_draft(st, app, false);
    }
    let not_default = (app.config.ui_scale - 1.0).abs() > 0.001;
    let reset = ui.add_enabled(not_default, egui::Button::new(icons::RESTART).min_size(egui::vec2(reset_w, 22.0)));
    if reset.on_hover_text("Reset to 100 % (Ctrl+0)").on_disabled_hover_text("Already at 100 %").clicked() {
        st.scale_draft = None;
        app.config.ui_scale = 1.0;
    }
}

fn accent_picker(ui: &mut Ui, app: &mut AppState) {
    ui.spacing_mut().interact_size.x = 64.0;
    let mut rgb = app.config.accent;
    if ui.color_edit_button_srgb(&mut rgb).on_hover_text("Pick any colour").changed() {
        app.config.accent = rgb;
    }
    ui.add_space(8.0);
    for (name, preset) in theme::ACCENT_PRESETS {
        if swatch(ui, preset, app.config.accent == preset).on_hover_text(name).clicked() {
            app.config.accent = preset;
        }
    }
    let name = theme::ACCENT_PRESETS.iter().find(|(_, c)| *c == app.config.accent).map_or("Custom", |(n, _)| *n);
    ui.add_space(4.0);
    ui.label(RichText::new(name).color(Palette::TEXT_DIM));
}

fn swatch(ui: &mut Ui, rgb: [u8; 3], selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect.shrink(3.0), CornerRadius::same(3), Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        let ring = if selected {
            Some(Stroke::new(1.5_f32, Color32::WHITE))
        } else if response.hovered() {
            Some(Stroke::new(1.0_f32, Color32::from_gray(150)))
        } else {
            None
        };
        if let Some(stroke) = ring {
            painter.rect_stroke(rect.shrink(0.5), CornerRadius::same(5), stroke, StrokeKind::Inside);
        }
    }
    response
}

// --- Bonsai server ---------------------------------------------------------------------------

fn server_page(ui: &mut Ui, st: &mut SettingsState, app: &mut AppState) {
    widgets::panel(ui, "settings-folder", "Bonsai Folder", true, |ui| {
        widgets::prop_row(ui, "Folder", |ui| {
            let text = app.config.bonsai_dir.as_ref().map_or("Not chosen yet".to_string(), |d| d.display().to_string());
            let label = format!("{} Change...", icons::FOLDER);
            let button_w = button_width(ui, &label, 92.0);
            let width = ui.available_width() - button_w - ui.spacing().item_spacing.x;
            path_field(ui, &text, width);
            let change = ui
                .add(egui::Button::new(label).min_size(egui::vec2(button_w, 22.0)))
                .on_hover_text("Choose another Bonsai-demo folder (also Bonsai > Change Bonsai folder...)");
            if change.clicked() {
                app.show_setup = true;
            }
        });
        widgets::prop_row(ui, "Status", |ui| install_status(ui, app));
    });

    widgets::panel(ui, "settings-connection", "Connection", true, |ui| {
        widgets::prop_row(ui, "Server URL", |ui| url_field(ui, st, app));
        let invalid = st.url_draft.as_deref().is_some_and(|u| !valid_url(u));
        if invalid {
            hint_colored(ui, "The address must start with http:// or https://", Palette::ERR);
        } else {
            hint(ui, "The Bonsai launcher always serves on port 8080. Press Enter to apply, Esc to undo.");
        }
    });

    let c = &mut app.config;
    widgets::panel(ui, "settings-start-stop", "Start and Stop", true, |ui| {
        widgets::prop_row(ui, "Auto-start", |ui| {
            ui.checkbox(&mut c.auto_start, "Start Bonsai when a prompt or chat needs it")
                .on_hover_text("Off: turn Bonsai on yourself with the switch in the top bar");
        });
        widgets::prop_row(ui, "Auto-stop", |ui| {
            let drag = egui::DragValue::new(&mut c.auto_stop_idle_minutes)
                .range(0..=240)
                .clamp_existing_to_range(false)
                .speed(0.2)
                .custom_formatter(|v, _| if v < 0.5 { "Never".to_string() } else { format!("after {v:.0} idle min") })
                .custom_parser(parse_number);
            number_field(ui, drag, "Drag or double-click to type minutes (0 = never)");
        });
        hint(ui, "Frees the GPU for ComfyUI when this app started Bonsai and nothing ran for that long.");
        widgets::prop_row(ui, "On exit", |ui| {
            ui.checkbox(&mut c.stop_on_exit, "Stop Bonsai when the app closes")
                .on_hover_text("Only when this app started it; a Bonsai started with start_bonsai.bat keeps running");
        });
        widgets::prop_row(ui, "VRAM busy limit", |ui| {
            let drag = egui::DragValue::new(&mut c.vram_busy_mb)
                .range(VRAM_BUSY_RANGE)
                .clamp_existing_to_range(false)
                .speed(10.0)
                .custom_formatter(|v, _| if v < 0.5 { "Off - always start".to_string() } else { format!("{v:.0} MB") })
                .custom_parser(parse_number);
            number_field(ui, drag, "Drag or double-click to type MB (0 = off)");
        });
        hint(
            ui,
            "Auto-start waits while other programs (e.g. ComfyUI) use more VRAM than this. \
             The power switch and Bonsai > Start always start.",
        );
    });

    widgets::panel(ui, "settings-launch", "Launch Options", true, |ui| {
        hint_colored(
            ui,
            &format!("{} These apply the next time Bonsai starts (Bonsai > Restart applies them now).", icons::INFO),
            Palette::INFO,
        );
        widgets::prop_row(ui, "Context size", |ui| {
            let options = [(0, "Auto"), (8192, "8192"), (16384, "16384"), (32768, "32768"), (65536, "65536")];
            widgets::enum_buttons(ui, &mut c.ctx_size, &options);
        });
        context_note(ui, c.ctx_size);
        widgets::prop_row(ui, "Vision projector", |ui| {
            ui.checkbox(&mut c.mmproj_cpu, "Run on the CPU")
                .on_hover_text("BONSAI_MMPROJ_CPU=1: frees about 0.9 GB of VRAM; reading images gets slower");
        });
        widgets::prop_row(ui, "Start timeout", |ui| {
            let drag = egui::DragValue::new(&mut c.start_timeout_secs)
                .range(30..=1800)
                .clamp_existing_to_range(false)
                .speed(1.0)
                .suffix(" s");
            number_field(ui, drag, "How long a prompt waits for the model to load before giving up");
        });
        widgets::prop_row(ui, "Environment", |ui| env_editor(ui, &mut c.extra_env));
    });
}

/// Number typed into a formatted field ("after 12 idle min" -> 12, "Never" / "Off" -> 0).
fn parse_number(text: &str) -> Option<f64> {
    match text.find(|c: char| c.is_ascii_digit()) {
        Some(start) => {
            let digits: String = text[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            digits.parse().ok()
        }
        None => {
            let lower = text.trim().to_ascii_lowercase();
            (lower.starts_with('n') || lower.starts_with('o')).then_some(0.0)
        }
    }
}

fn install_status(ui: &mut Ui, app: &AppState) {
    ui.vertical(|ui| match &app.status.install {
        None => {
            let text = format!("{} No folder chosen - the app cannot start Bonsai", icons::WARN);
            ui.label(RichText::new(text).color(Palette::WARN));
        }
        Some(install) if app.status.install_ok => {
            let model = install
                .primary_model()
                .and_then(|m| m.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} Ready to start", icons::CHECK)).color(Palette::OK));
                let detail = format!("{} · {model}", install.backend().unwrap_or("?"));
                ui.add(egui::Label::new(RichText::new(detail).color(Palette::TEXT_DIM)).truncate());
            });
        }
        Some(install) => {
            for problem in install.problems() {
                ui.add(egui::Label::new(RichText::new(format!("✖ {problem}")).color(Palette::ERR)).wrap());
            }
        }
    });
}

fn valid_url(url: &str) -> bool {
    let url = url.trim();
    (url.starts_with("http://") || url.starts_with("https://")) && url.len() > "http://".len()
}

fn url_field(ui: &mut Ui, st: &mut SettingsState, app: &mut AppState) {
    let mut text = st.url_draft.clone().unwrap_or_else(|| app.config.server_url.clone());
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id_salt("bh3-settings-url")
            .font(theme::mono(12.5))
            .hint_text(AppConfig::default().server_url)
            .desired_width(f32::INFINITY),
    );
    if response.changed() {
        st.url_draft = Some(text);
    }
    if response.lost_focus() {
        commit_url_draft(st, app, ui.input(|i| i.key_pressed(Key::Escape)));
    }
}

fn context_note(ui: &mut Ui, ctx_size: u32) {
    let (text, color) = match ctx_size {
        0 => {
            ("Auto: the launcher picks the context from your RAM (16384 on a 16 GB PC).".to_string(), Palette::TEXT_DIM)
        }
        n => {
            let gib = n as f64 * 64.0 / (1024.0 * 1024.0);
            let custom = if [8192, 16384, 32768, 65536].contains(&n) { "" } else { " (custom value)" };
            let mut text =
                format!("{n} tokens{custom}: the KV cache takes about {gib:.1} GiB of VRAM (~64 KiB per token).");
            let color = if n >= 32768 {
                text.push_str(" May not fit next to the 27B model on an 8 GB GPU.");
                Palette::WARN
            } else {
                Palette::TEXT_DIM
            };
            (text, color)
        }
    };
    hint_colored(ui, &text, color);
}

fn env_editor(ui: &mut Ui, vars: &mut Vec<EnvVar>) {
    ui.vertical(|ui| {
        let mut remove = None;
        // Exact widths, so the rows never push the panel wider: key = value field, "=" and the
        // remove button (an icon button: at least 24 wide) fixed.
        let spacing = ui.spacing().item_spacing.x;
        let eq_w = ui.painter().layout_no_wrap("=".into(), FontId::proportional(13.5), Palette::TEXT_DIM).size().x;
        let remove_w = button_width(ui, "✖", 24.0);
        let field_w = ((ui.available_width() - eq_w - remove_w - 3.0 * spacing) / 2.0).floor().max(60.0);
        for (index, var) in vars.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let key = egui::TextEdit::singleline(&mut var.key)
                    .id_salt(("bh3-env-key", index))
                    .font(theme::mono(12.5))
                    .hint_text("NAME")
                    .vertical_align(Align::Center)
                    .desired_width(f32::INFINITY);
                ui.add_sized([field_w, 22.0], key);
                ui.label(RichText::new("=").color(Palette::TEXT_DIM));
                let value = egui::TextEdit::singleline(&mut var.value)
                    .id_salt(("bh3-env-value", index))
                    .font(theme::mono(12.5))
                    .hint_text("value")
                    .vertical_align(Align::Center)
                    .desired_width(f32::INFINITY);
                ui.add_sized([field_w, 22.0], value);
                if widgets::icon_button(ui, "✖", "Remove this variable").clicked() {
                    remove = Some(index);
                }
            });
            if let Some(problem) = env_key_problem(&var.key) {
                ui.label(RichText::new(problem).size(HINT_SIZE).color(Palette::WARN));
            }
        }
        if let Some(index) = remove {
            vars.remove(index);
        }
        let add = ui
            .add(egui::Button::new(format!("{} Add variable", icons::ADD)).min_size(egui::vec2(0.0, 22.0)))
            .on_hover_text("Pass another environment variable to the Bonsai launcher");
        if add.clicked() {
            vars.push(EnvVar::default());
        }
        ui.label(
            RichText::new("e.g. BONSAI_KV4=1 (4-bit KV cache) or BONSAI_IMAGE_MAX_TOKENS=1024")
                .size(HINT_SIZE)
                .color(Palette::TEXT_DIM),
        );
    });
}

fn env_key_problem(key: &str) -> Option<&'static str> {
    let key = key.trim();
    if key.contains(|c: char| c.is_whitespace() || c == '=') {
        Some("A name cannot contain spaces or '='")
    } else if key.eq_ignore_ascii_case("BONSAI_CTX") || key.eq_ignore_ascii_case("BONSAI_MMPROJ_CPU") {
        Some("Also set by the options above - this row wins")
    } else {
        None
    }
}

// --- Prompt writing / Chat -------------------------------------------------------------------

fn writing_page(ui: &mut Ui, app: &mut AppState) {
    let c = &mut app.config;
    widgets::panel(ui, "settings-writer-gen", "Generation", true, |ui| {
        params_header(ui, &mut c.writer_params, &GenParams::prompt_writer(), "Reset to tested defaults");
        gen_params_ui(ui, &mut c.writer_params, &WRITER_RANGES);
    });
    widgets::panel(ui, "settings-writer-requests", "Requests", true, |ui| {
        widgets::prop_row(ui, "Default duration", |ui| {
            let labels: Vec<(u32, String)> = lengths::DURATIONS.iter().map(|d| (*d, format!("{d} s"))).collect();
            let options: Vec<(u32, &str)> = labels.iter().map(|(d, l)| (*d, l.as_str())).collect();
            widgets::enum_buttons(ui, &mut c.default_duration, &options);
        });
        let seconds = c.default_duration as f32;
        let frames = lengths::frames_for_seconds(seconds);
        let mut text = format!(
            "H3 renders {} s as {} s ({frames} frames at 24 fps); prompts use the real length.",
            c.default_duration,
            lengths::format_real(seconds)
        );
        if !lengths::DURATIONS.contains(&c.default_duration) {
            text.push_str(" (custom value from config.json)");
        }
        hint(ui, &text);
        widgets::prop_row(ui, "History", |ui| {
            ui.checkbox(&mut c.save_history, "Save every written prompt")
                .on_hover_text("Saved prompts are listed in the History workspace");
        });
    });
}

fn chat_page(ui: &mut Ui, app: &mut AppState) {
    let c = &mut app.config;
    widgets::panel(ui, "settings-chat-gen", "Generation", true, |ui| {
        params_header(ui, &mut c.chat_params, &GenParams::chat(), "Reset to chat defaults");
        gen_params_ui(ui, &mut c.chat_params, &CHAT_RANGES);
        hint(ui, "Used by the Chat workspace. Prompts written in Write use Prompt writing.");
    });
}

/// "Tested defaults" / "Changed" badge and the reset button above a generation editor.
fn params_header(ui: &mut Ui, params: &mut GenParams, defaults: &GenParams, reset_label: &str) {
    ui.horizontal(|ui| {
        ui.add_space(label_column(ui));
        let is_default = params == defaults;
        if is_default {
            widgets::badge(ui, &format!("{} Defaults", icons::CHECK), Palette::OK);
        } else {
            widgets::badge(ui, "Changed", Palette::WARN);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let button = egui::Button::new(format!("{} {reset_label}", icons::RESTART)).min_size(egui::vec2(0.0, 22.0));
            let tip = if is_default {
                "Already at the defaults"
            } else {
                "Thinking off, temperature, sampling and max tokens"
            };
            if ui.add_enabled(!is_default, button).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
                *params = defaults.clone();
            }
        });
    });
    ui.add_space(4.0);
}

fn gen_params_ui(ui: &mut Ui, p: &mut GenParams, ranges: &ParamRanges) {
    widgets::prop_row(ui, "Thinking", |ui| {
        ui.vertical(|ui| {
            let options: Vec<(ThinkingMode, &str)> = ThinkingMode::all().into_iter().map(|m| (m, m.label())).collect();
            widgets::enum_buttons(ui, &mut p.thinking, &options);
            let color = if p.thinking == ThinkingMode::Off { Palette::TEXT_DIM } else { Palette::WARN };
            ui.label(RichText::new(p.thinking.hint()).size(HINT_SIZE).color(color));
        });
    });
    slider_row(
        ui,
        "Temperature",
        &mut p.temperature,
        ranges.temperature.clone(),
        2,
        "Randomness: 0.4 is tested for prompts; higher = more varied",
    );
    widgets::prop_row(ui, "Max tokens", |ui| {
        fit_slider(ui, 0.0);
        let slider = egui::Slider::new(&mut p.max_tokens, ranges.max_tokens.clone())
            .logarithmic(true)
            .clamping(egui::SliderClamping::Edits);
        ui.add(slider).on_hover_text("Most new tokens per answer (reasoning + text)");
    });
    slider_row(
        ui,
        "Top-p",
        &mut p.top_p,
        0.0..=1.0,
        2,
        "Nucleus sampling: keep the most likely tokens up to this total",
    );
    let top_k_tip = "Choose only among the k most likely tokens (0 = off)";
    slider_row(ui, "Top-k", &mut p.top_k, ranges.top_k.clone(), 0, top_k_tip);
    slider_row(ui, "Min-p", &mut p.min_p, 0.0..=1.0, 2, "Drop tokens less likely than this share of the best one");
    widgets::prop_row(ui, "Seed", |ui| {
        let mut fixed = p.seed.is_some();
        let mut seed = p.seed.unwrap_or(42);
        let toggle = ui
            .checkbox(&mut fixed, "Fixed")
            .on_hover_text("Fixed: the same request and settings give the same text. Off: a new variation each run");
        if toggle.changed() {
            p.seed = fixed.then_some(seed);
        }
        let field = ui.add_enabled_ui(fixed, |ui| {
            let drag = egui::DragValue::new(&mut seed)
                .range(0..=i64::from(u32::MAX))
                .clamp_existing_to_range(false)
                .speed(1.0);
            ui.add_sized([ui.available_width(), 22.0], drag)
        });
        if field.inner.changed() && fixed {
            p.seed = Some(seed);
        }
    });
}

// --- Data ------------------------------------------------------------------------------------

fn data_page(ui: &mut Ui, st: &mut SettingsState, app: &mut AppState) {
    if st.data.as_ref().is_none_or(|d| app.time - d.read_at > DATA_REFRESH_SECS) {
        st.data = Some(DataInfo::read(app.time));
    }
    let Some(info) = st.data.as_ref() else { return };
    let edited = LibraryItem::all().iter().filter(|item| app.library.is_overridden(**item)).count();
    let mut open: Option<(PathBuf, bool)> = None;

    widgets::panel(ui, "settings-folders", "Folders and Files", true, |ui| {
        let rows = [
            ("Settings", &info.config_dir, "config.json".to_string(), true, false),
            ("History", &info.history_dir, count_text(info.history_count, "prompt", "prompts"), true, false),
            ("Library", &info.library_dir, count_text(edited, "edited text", "edited texts"), true, true),
            (
                "Server log",
                &info.log_path,
                info.log_size.map_or("not written yet".to_string(), fmt_size),
                info.log_size.is_some(),
                false,
            ),
        ];
        // One column for the details (right-aligned), so the path fields line up.
        let detail_font = FontId::proportional(HINT_SIZE);
        let detail_w = rows
            .iter()
            .map(|r| ui.painter().layout_no_wrap(r.2.clone(), detail_font.clone(), Palette::TEXT_DIM).size().x)
            .fold(0.0_f32, f32::max)
            .ceil();
        let open_label = format!("{} Open", icons::FOLDER);
        let button_w = button_width(ui, &open_label, 70.0);
        for (label, path, detail, enabled, create) in rows {
            widgets::prop_row(ui, label, |ui| {
                let spacing = ui.spacing().item_spacing.x;
                let width = ui.available_width() - button_w - detail_w - 2.0 * spacing;
                path_field(ui, &path.display().to_string(), width);
                ui.allocate_ui_with_layout(egui::vec2(detail_w, 22.0), Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(detail).font(detail_font.clone()).color(Palette::TEXT_DIM));
                });
                let tip = if enabled { format!("Open {}", path.display()) } else { "Nothing to open yet".to_string() };
                let button = egui::Button::new(open_label.as_str()).min_size(egui::vec2(button_w, 22.0));
                if ui.add_enabled(enabled, button).on_hover_text(&tip).on_disabled_hover_text(&tip).clicked() {
                    open = Some((path.to_path_buf(), create));
                }
            });
        }
        hint(ui, "Delete the settings folder (with the app closed) to start from scratch.");
    });

    widgets::panel(ui, "settings-reset", "Reset", true, |ui| {
        widgets::prop_row(ui, "All settings", |ui| {
            let button = egui::Button::new(RichText::new("Reset all settings...").color(Color32::WHITE))
                .fill(Palette::ERR.lerp_to_gamma(Palette::WIDGET, 0.45))
                .min_size(egui::vec2(170.0, 22.0));
            if ui.add(button).on_hover_text("Asks before resetting").clicked() {
                st.confirm_reset = true;
            }
        });
        hint(
            ui,
            "Every setting goes back to its default. The Bonsai folder is kept; history, chats and \
             library edits are not touched.",
        );
    });

    if let Some((path, create)) = open {
        if create {
            if let Err(e) = std::fs::create_dir_all(&path) {
                app.toast(ToastKind::Error, format!("Could not create {}: {e}", path.display()));
                return;
            }
        }
        if let Err(e) = util::open_path(&path) {
            app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", path.display()));
        }
    }
}

/// "no prompts" / "1 prompt" / "3 prompts".
fn count_text(n: usize, one: &str, many: &str) -> String {
    match n {
        0 => format!("no {many}"),
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

fn fmt_size(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb >= 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{kb:.0} KB")
    }
}

fn reset_dialog(ctx: &egui::Context, st: &mut SettingsState, app: &mut AppState) {
    if !st.confirm_reset {
        return;
    }
    let folder = app.config.bonsai_dir.as_ref().map(|d| d.display().to_string());
    let modal = egui::Modal::new(Id::new("bh3-settings-reset")).frame(widgets::dialog_frame()).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.style_mut().interaction.selectable_labels = false;
        ui.label(RichText::new(format!("{} Reset all settings?", icons::WARN)).size(15.5).strong().color(Palette::TEXT));
        ui.add_space(6.0);
        ui.label("Interface, Bonsai launch options and the generation settings for prompts and chat go back to their defaults.");
        ui.add_space(4.0);
        let kept = match &folder {
            Some(dir) => format!("Kept: the Bonsai folder ({dir}), history, chats and library edits."),
            None => "Kept: history, chats and library edits.".to_string(),
        };
        ui.label(RichText::new(kept).color(Palette::TEXT_DIM));
        ui.add_space(14.0);
        let mut choice = None;
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let reset = egui::Button::new(RichText::new("Reset").strong().color(Color32::WHITE))
                    .fill(Palette::ERR.lerp_to_gamma(Palette::WIDGET, 0.3))
                    .min_size(egui::vec2(90.0, 26.0));
                if ui.add(reset).clicked() {
                    choice = Some(true);
                }
                if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(90.0, 26.0))).on_hover_text("Esc").clicked() {
                    choice = Some(false);
                }
            });
        });
        choice
    });
    match modal.inner {
        Some(true) => {
            let bonsai_dir = app.config.bonsai_dir.clone();
            app.config = AppConfig { bonsai_dir, ..AppConfig::default() };
            st.scale_draft = None;
            st.url_draft = None;
            st.confirm_reset = false;
            app.toast(ToastKind::Ok, "All settings reset (Bonsai folder kept)");
        }
        Some(false) => st.confirm_reset = false,
        None if modal.should_close() => st.confirm_reset = false,
        None => {}
    }
}

// --- About -----------------------------------------------------------------------------------

fn about_page(ui: &mut Ui) {
    egui::Frame::new()
        .fill(Palette::PANEL_BODY)
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(widgets::app_icon(64.0));
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new(bh3::APP_NAME).size(20.0).strong().color(Palette::TEXT));
                    ui.label(RichText::new(format!("Version {}", bh3::VERSION)).color(Palette::TEXT_DIM));
                    ui.label(
                        RichText::new("MIT License · © 2026 Bonsai H3 Writer contributors")
                            .size(HINT_SIZE)
                            .color(Palette::TEXT_FAINT),
                    );
                });
            });
            ui.add_space(10.0);
            ui.label(
                "Writes MiniMax H3 video prompts - r2v (reference images) and f2v (first frame) - with your \
                 local Bonsai 27B, checks them against the official guides, and starts and stops Bonsai so \
                 ComfyUI gets the GPU back.",
            );
        });
    ui.add_space(6.0);
    widgets::panel(ui, "settings-guides", "Official H3 Guides", true, |ui| {
        widgets::prop_row(ui, "Reference (r2v)", |ui| {
            ui.hyperlink_to("VIDEO_PROMPT_WRITING_GUIDE_ref_en.md", bh3::GUIDE_REF_URL);
        });
        widgets::prop_row(ui, "First frame (f2v)", |ui| {
            ui.hyperlink_to("VIDEO_PROMPT_WRITING_GUIDE_base_en.md", bh3::GUIDE_BASE_URL);
        });
    });
    widgets::panel(ui, "settings-tested", "Why These Defaults", true, |ui| {
        let lines = [
            "Thinking off and temperature 0.4 gave valid prompts in 10-25 s on the RTX 4060 Laptop GPU (8 GB).",
            "Thinking Low or High burned 1-3k reasoning tokens without answering, so thinking is off by default.",
            "The system prompt stays byte-identical, so llama-server keeps it cached and only the new request is processed.",
            "Prompts: temperature 0.4, top-p 0.95, top-k 20, min-p 0.05, 1500 max tokens. Chat: temperature 0.7, 2048 max tokens.",
        ];
        for line in lines {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new("•").color(theme::accent(ui.ctx())));
                ui.add(egui::Label::new(line).wrap());
            });
            ui.add_space(2.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_names() {
        assert_eq!(Category::from_name("Bonsai"), Some(Category::Server));
        assert_eq!(Category::from_name(" prompt "), Some(Category::Writing));
        assert_eq!(Category::from_name("nope"), None);
    }

    #[test]
    fn number_parsing() {
        assert_eq!(parse_number("after 12 idle min"), Some(12.0));
        assert_eq!(parse_number("12"), Some(12.0));
        assert_eq!(parse_number("1500 MB"), Some(1500.0));
        assert_eq!(parse_number("Never"), Some(0.0));
        assert_eq!(parse_number("Off - always start"), Some(0.0));
        assert_eq!(parse_number("x"), None);
    }

    #[test]
    fn env_names() {
        assert!(env_key_problem("BONSAI KV4").is_some());
        assert!(env_key_problem("BONSAI_KV4").is_none());
        assert!(env_key_problem("").is_none());
    }

    #[test]
    fn urls() {
        assert!(valid_url("http://127.0.0.1:8080"));
        assert!(!valid_url("127.0.0.1:8080"));
        assert!(!valid_url("http://"));
    }

    #[test]
    fn url_drafts() {
        let mut config = AppConfig::default();
        let old = config.server_url.clone();
        assert_eq!(apply_url_draft(&mut config, "http://127.0.0.1:9090", true), None);
        assert_eq!(apply_url_draft(&mut config, &format!("{old}/"), false), None);
        assert_eq!(apply_url_draft(&mut config, "127.0.0.1:9090", false).map(|t| t.0), Some(ToastKind::Warn));
        assert_eq!(config.server_url, old);
        assert_eq!(apply_url_draft(&mut config, " http://127.0.0.1:9090/ ", false).map(|t| t.0), Some(ToastKind::Ok));
        assert_eq!(config.server_url, "http://127.0.0.1:9090");
    }

    #[test]
    fn ranges_hold_the_defaults() {
        for (ranges, defaults) in [(&WRITER_RANGES, GenParams::prompt_writer()), (&CHAT_RANGES, GenParams::chat())] {
            assert!(ranges.temperature.contains(&defaults.temperature));
            assert!(ranges.max_tokens.contains(&defaults.max_tokens));
            assert!(ranges.top_k.contains(&defaults.top_k));
        }
        assert!(VRAM_BUSY_RANGE.contains(&AppConfig::default().vram_busy_mb));
    }

    #[test]
    fn drawing_the_generation_editor_keeps_stored_values() {
        // Set elsewhere (by hand in config.json) outside every range, and not on a slider step.
        let mut params =
            GenParams { temperature: 2.345, max_tokens: 50_000, top_k: 900, seed: Some(1 << 40), ..GenParams::chat() };
        let before = params.clone();
        let ctx = egui::Context::default();
        for _ in 0..3 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| gen_params_ui(ui, &mut params, &CHAT_RANGES));
            });
        }
        assert_eq!(params, before);
    }
}
