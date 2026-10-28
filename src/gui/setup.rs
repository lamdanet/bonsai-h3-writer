//! First-run dialog (also Bonsai > Change Bonsai folder...): pick the Bonsai-demo folder from the
//! installs found in the usual places or with Browse..., see what each one contains, then use it.

use std::path::{Path, PathBuf};

use bh3::server::{self, BonsaiInstall};
use eframe::egui::{
    self, text::LayoutJob, Align, Color32, CornerRadius, FontId, Id, Key, Layout, Margin, RichText, Sense, Stroke,
    StrokeKind, TextFormat, Ui,
};

use super::app::{AppState, ToastKind};
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Width of the dialog content.
const WIDTH: f32 = 620.0;

/// Dialog state; the folders are scanned once each time the dialog opens.
#[derive(Default)]
pub struct SetupState {
    cards: Vec<Card>,
    /// Root of the selected install.
    selected: Option<PathBuf>,
    /// Browse... picked a folder that is not a Bonsai install.
    browse_error: Option<String>,
    /// "Use this folder" failed.
    use_error: Option<String>,
    /// egui pass in which the dialog was last drawn (a gap means it was opened again).
    last_pass: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Ok,
    Warn,
    Bad,
}

impl Mark {
    fn glyph(self) -> &'static str {
        match self {
            Mark::Ok => icons::CHECK,
            Mark::Warn => icons::WARN,
            Mark::Bad => "✖",
        }
    }
    fn color(self) -> Color32 {
        match self {
            Mark::Ok => Palette::OK,
            Mark::Warn => Palette::WARN,
            Mark::Bad => Palette::ERR,
        }
    }
}

/// One line of a card: mark, what, found value and a dim detail.
struct Item {
    mark: Mark,
    label: &'static str,
    value: String,
    detail: String,
}

/// A detected folder with its display lines (computed once: sizes need file system calls).
struct Card {
    install: BonsaiInstall,
    items: Vec<Item>,
    problems: Vec<String>,
    notes: Vec<String>,
    /// `BH3_DEMO=1` sample, never selectable for use.
    demo: bool,
}

impl Card {
    fn new(install: BonsaiInstall) -> Self {
        let root = install.root.clone();
        let rel = |p: &Path| p.strip_prefix(&root).unwrap_or(p).display().to_string();
        let file = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let item = |mark, label, value: String, detail: String| Item { mark, label, value, detail };
        let mut items = Vec::new();

        items.push(match (&install.launcher_ps1, &install.server_exe) {
            (Some(p), _) => {
                let bat = if install.launcher_bat.is_some() { "+ start_bonsai.bat" } else { "" };
                item(Mark::Ok, "Launcher", rel(p), bat.to_string())
            }
            (None, Some(_)) => item(
                Mark::Warn,
                "Launcher",
                r"scripts\start_llama_server.ps1 not found".into(),
                "llama-server.exe is started directly".into(),
            ),
            (None, None) => {
                item(Mark::Bad, "Launcher", r"scripts\start_llama_server.ps1 not found".into(), String::new())
            }
        });
        items.push(match &install.server_exe {
            Some(p) => item(Mark::Ok, "Server", rel(p), install.backend().unwrap_or_default().to_string()),
            None => {
                item(Mark::Bad, "Server", "llama-server.exe not found".into(), r"bin\cuda, hip, vulkan or cpu".into())
            }
        });
        items.push(match install.primary_model() {
            Some(m) => {
                let mut detail = std::fs::metadata(m).map(|md| fmt_bytes(md.len())).unwrap_or_default();
                let more = install.models.len().saturating_sub(1);
                if more > 0 {
                    detail.push_str(&format!(" · +{more} more"));
                }
                item(Mark::Ok, "Model", file(m), detail)
            }
            None => item(Mark::Bad, "Model", r"no *.gguf under models\".into(), String::new()),
        });
        items.push(match &install.mmproj {
            Some(p) => item(Mark::Ok, "Vision", file(p), "image input".into()),
            None => item(Mark::Warn, "Vision", "no *mmproj*.gguf".into(), "image input disabled".into()),
        });

        let problems = install.problems();
        // Warnings already shown as a line above are left out.
        let notes = install
            .warnings()
            .into_iter()
            .filter(|w| !w.contains("mmproj") && !w.contains("started directly"))
            .collect();
        Self { install, items, problems, notes, demo: false }
    }

    /// `BH3_DEMO=1`: an old, broken copy, so the screenshot shows how problems look.
    fn demo() -> Self {
        let root = PathBuf::from(r"E:\Old\Bonsai-demo");
        let install = BonsaiInstall {
            launcher_ps1: Some(root.join("scripts").join("start_llama_server.ps1")),
            launcher_bat: Some(root.join("start_bonsai.bat")),
            root,
            ..BonsaiInstall::default()
        };
        let mut card = Self::new(install);
        card.demo = true;
        card
    }

    fn usable(&self) -> bool {
        !self.demo && self.install.is_usable()
    }
}

fn fmt_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Same folder, ignoring case and trailing separators (Windows paths).
fn same_dir(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| p.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase();
    key(a) == key(b)
}

impl SetupState {
    fn rescan(&mut self, configured: Option<&Path>) {
        let mut cards: Vec<Card> = server::find_installs().into_iter().map(Card::new).collect();
        if let Some(dir) = configured {
            if !cards.iter().any(|c| same_dir(&c.install.root, dir)) {
                if let Ok(install) = BonsaiInstall::detect(dir) {
                    cards.insert(0, Card::new(install));
                }
            }
        }
        if widgets::demo_mode() {
            cards.push(Card::demo());
        }
        // Keep the selection on a rescan; else the configured folder; else the first usable one.
        let find = |dir: &Path| cards.iter().find(|c| same_dir(&c.install.root, dir));
        self.selected = self
            .selected
            .as_deref()
            .and_then(find)
            .or_else(|| configured.and_then(find))
            .or_else(|| cards.iter().find(|c| c.usable()))
            .map(|c| c.install.root.clone());
        self.cards = cards;
        self.browse_error = None;
        self.use_error = None;
    }

    fn selected_card(&self) -> Option<&Card> {
        let selected = self.selected.as_deref()?;
        self.cards.iter().find(|c| same_dir(&c.install.root, selected))
    }

    fn browse(&mut self, fallback: Option<&Path>) {
        let start = self.selected.clone().or_else(|| fallback.map(Path::to_path_buf));
        let Some(dir) = util::pick_folder("Choose the Bonsai-demo folder", start.as_deref()) else { return };
        match BonsaiInstall::detect(&dir) {
            Ok(install) => {
                let root = install.root.clone();
                self.cards.retain(|c| !same_dir(&c.install.root, &root));
                self.cards.insert(0, Card::new(install));
                self.selected = Some(root);
                self.browse_error = None;
            }
            Err(e) => self.browse_error = Some(format!("{e:#}")),
        }
        self.use_error = None;
    }
}

enum Outcome {
    Use(PathBuf),
    Skip,
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    let mut st = std::mem::take(&mut app.setup);
    let pass = ctx.cumulative_pass_nr();
    if st.last_pass.is_none_or(|last| last + 1 < pass) {
        st.selected = None;
        st.rescan(app.config.bonsai_dir.as_deref());
    }
    st.last_pass = Some(pass);

    let outcome = dialog(ctx, &mut st, app);
    match outcome {
        Some(Outcome::Use(root)) => match app.set_bonsai_dir(root) {
            Ok(install) => {
                app.show_setup = false;
                st.last_pass = None;
                app.toast(ToastKind::Ok, format!("Bonsai folder: {}", install.root.display()));
            }
            Err(e) => st.use_error = Some(format!("{e:#}")),
        },
        Some(Outcome::Skip) => {
            app.show_setup = false;
            st.last_pass = None;
            if app.config.bonsai_dir.is_none() {
                app.toast(
                    ToastKind::Info,
                    "No Bonsai folder yet - choose it any time in Bonsai > Change Bonsai folder...",
                );
            }
        }
        None => {}
    }
    app.setup = st;
}

fn dialog(ctx: &egui::Context, st: &mut SetupState, app: &AppState) -> Option<Outcome> {
    let mut outcome = None;
    let screen_h = ctx.screen_rect().height();
    let modal = egui::Modal::new(Id::new("bh3-setup"))
        .backdrop_color(Color32::from_black_alpha(150))
        .frame(widgets::dialog_frame().inner_margin(0))
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            ui.style_mut().interaction.selectable_labels = false;
            header(ui);
            egui::Frame::new().inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 12 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                intro(ui, app.config.auto_stop_idle_minutes);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    widgets::section_label(ui, "Bonsai-demo folders found");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let rescan = ui
                            .add(
                                egui::Button::new(format!("{} Rescan", icons::RESTART)).min_size(egui::vec2(0.0, 22.0)),
                            )
                            .on_hover_text(r"Look again in D:\, C:\, E:\, your user folder, Documents and Desktop");
                        if rescan.clicked() {
                            st.rescan(app.config.bonsai_dir.as_deref());
                        }
                    });
                });
                ui.add_space(2.0);
                let max_h = (screen_h - 330.0).clamp(140.0, 440.0);
                egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                    cards(ui, st, app, &mut outcome);
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let browse = ui
                        .add(egui::Button::new(format!("{} Browse...", icons::FOLDER)).min_size(egui::vec2(96.0, 24.0)))
                        .on_hover_text("Choose the Bonsai-demo folder yourself");
                    if browse.clicked() {
                        st.browse(app.config.bonsai_dir.as_deref());
                    }
                    ui.label(RichText::new("Pick the folder that contains start_bonsai.bat").color(Palette::TEXT_DIM));
                });
            });
            footer(ui, st, &mut outcome);
        });

    if modal.is_top_modal && !modal.any_popup_open {
        let (esc, enter) =
            ctx.input_mut(|i| (i.consume_key(egui::Modifiers::NONE, Key::Escape), i.key_pressed(Key::Enter)));
        if esc {
            outcome = Some(Outcome::Skip);
        } else if enter && outcome.is_none() {
            if let Some(card) = st.selected_card().filter(|c| c.usable()) {
                outcome = Some(Outcome::Use(card.install.root.clone()));
            }
        }
    }
    outcome
}

fn header(ui: &mut Ui) {
    egui::Frame::new()
        .fill(Palette::HEADER)
        .corner_radius(CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 })
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(widgets::app_icon(40.0));
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.add_space(1.0);
                    ui.label(RichText::new("Welcome to Bonsai H3 Writer").size(18.0).strong().color(Palette::TEXT));
                    ui.label(
                        RichText::new("Writes MiniMax H3 video prompts with your local Bonsai 27B")
                            .color(Palette::TEXT_DIM),
                    );
                });
            });
        });
    let (line, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(line, 0.0, Palette::OUTLINE);
}

/// The short explanation, with the key words in bold.
fn intro(ui: &mut Ui, idle_minutes: u32) {
    let normal = TextFormat { font_id: FontId::proportional(13.5), color: Palette::TEXT, ..Default::default() };
    let strong = TextFormat { color: Color32::WHITE, ..normal.clone() };
    let mono = TextFormat { font_id: theme::mono(12.5), color: Palette::TEXT, ..Default::default() };
    let stop = match idle_minutes {
        0 => "can stop Bonsai after idle minutes (Settings) to free it".to_string(),
        1 => "stops Bonsai after 1 idle minute to free it".to_string(),
        m => format!("stops Bonsai after {m} idle minutes to free it"),
    };
    let mut job = LayoutJob::default();
    job.append("Choose your ", 0.0, normal.clone());
    job.append("Bonsai-demo", 0.0, strong.clone());
    job.append(" folder - the one with ", 0.0, normal.clone());
    job.append("start_bonsai.bat", 0.0, mono);
    job.append(". The app starts and stops Bonsai by itself. Bonsai and ComfyUI ", 0.0, normal.clone());
    job.append("cannot share the 8 GB GPU", 0.0, strong);
    job.append(format!(", so the app {stop}.").as_str(), 0.0, normal);
    ui.label(job);
}

fn cards(ui: &mut Ui, st: &mut SetupState, app: &AppState, outcome: &mut Option<Outcome>) {
    if st.cards.is_empty() && st.browse_error.is_none() {
        egui::Frame::new()
            .fill(Palette::PANEL_BODY)
            .corner_radius(CornerRadius::same(5))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(
                        "No Bonsai-demo folder found in the usual places. Click Browse... and pick the folder \
                         that contains start_bonsai.bat.",
                    )
                    .color(Palette::TEXT_DIM),
                );
            });
    }
    let mut clicked = None;
    for card in &st.cards {
        let selected = st.selected.as_deref().is_some_and(|s| same_dir(&card.install.root, s));
        let current = app.config.bonsai_dir.as_deref().is_some_and(|d| same_dir(&card.install.root, d));
        let response = card_ui(ui, card, selected, current);
        if response.double_clicked() && card.usable() {
            *outcome = Some(Outcome::Use(card.install.root.clone()));
        } else if response.clicked() {
            clicked = Some(card.install.root.clone());
        }
        ui.add_space(6.0);
    }
    if let Some(root) = clicked {
        st.selected = Some(root);
        st.use_error = None;
    }
    if let Some(error) = &st.browse_error {
        egui::Frame::new()
            .fill(Palette::ERR.gamma_multiply(0.12))
            .stroke(Stroke::new(1.0_f32, Palette::ERR.gamma_multiply(0.7)))
            .corner_radius(CornerRadius::same(5))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(Mark::Bad.glyph()).color(Palette::ERR));
                    ui.add(egui::Label::new(RichText::new(error).color(Palette::ERR)).wrap());
                });
            });
    }
}

fn card_ui(ui: &mut Ui, card: &Card, selected: bool, current: bool) -> egui::Response {
    let usable = card.usable();
    let fill = if selected { Palette::PANEL_BODY.lerp_to_gamma(Palette::BLUE, 0.22) } else { Palette::PANEL_BODY };
    let stroke =
        if selected { Stroke::new(1.0_f32, Palette::BLUE_HOVER) } else { Stroke::new(1.0_f32, Palette::OUTLINE) };
    let shown = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin { left: 10, right: 10, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                radio(ui, selected);
                ui.add_space(2.0);
                let path = card.install.root.display().to_string();
                ui.add(egui::Label::new(RichText::new(path).strong().color(Palette::TEXT)).truncate());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (text, color) = if card.demo {
                        ("Sample", Palette::TEXT_DIM)
                    } else if usable {
                        ("Ready to use", Palette::OK)
                    } else {
                        ("Not usable", Palette::ERR)
                    };
                    widgets::badge(ui, text, color);
                    if current {
                        widgets::badge(ui, "Current", Palette::INFO);
                    }
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                egui::Grid::new(("bh3-setup-card", card.install.root.display().to_string()))
                    .num_columns(3)
                    .spacing(egui::vec2(8.0, 3.0))
                    .striped(false)
                    .show(ui, |ui| {
                        for item in &card.items {
                            ui.label(RichText::new(item.mark.glyph()).color(item.mark.color()));
                            ui.label(RichText::new(item.label).color(Palette::TEXT_DIM));
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let color = if item.mark == Mark::Bad { Palette::ERR } else { Palette::TEXT };
                                ui.add(egui::Label::new(RichText::new(&item.value).color(color)).truncate());
                                if !item.detail.is_empty() {
                                    ui.label(RichText::new(&item.detail).color(Palette::TEXT_FAINT));
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
            for problem in &card.problems {
                note_line(ui, Mark::Bad, problem);
            }
            for note in &card.notes {
                note_line(ui, Mark::Warn, note);
            }
        });
    let rect = shown.response.rect;
    let id = Id::new(("bh3-setup-card-click", card.install.root.display().to_string()));
    let response = ui.interact(rect, id, Sense::click());
    if response.hovered() && !selected {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(5),
            Stroke::new(1.0_f32, Color32::from_gray(96)),
            StrokeKind::Inside,
        );
    }
    let tip = if card.demo {
        "Sample entry (BH3_DEMO=1)"
    } else if usable {
        "Click to select, double-click to use this folder"
    } else {
        "This folder cannot start Bonsai: see the red lines"
    };
    response.on_hover_text(tip)
}

/// A wrapped red / yellow line under the items of a card.
fn note_line(ui: &mut Ui, mark: Mark, text: &str) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add_space(24.0);
        ui.label(RichText::new(mark.glyph()).color(mark.color()));
        ui.add(egui::Label::new(RichText::new(text).color(mark.color())).wrap());
    });
}

/// Blender-like radio dot.
fn radio(ui: &mut Ui, on: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::hover());
    let c = rect.center();
    if on {
        ui.painter().circle(c, 6.5, Palette::BLUE, Stroke::new(1.0_f32, Palette::BLUE_HOVER));
        ui.painter().circle_filled(c, 2.6, Color32::WHITE);
    } else {
        ui.painter().circle(c, 6.5, Palette::INPUT, Stroke::new(1.0_f32, Color32::from_gray(90)));
    }
}

fn footer(ui: &mut Ui, st: &SetupState, outcome: &mut Option<Outcome>) {
    let (line, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(line, 0.0, Palette::OUTLINE);
    egui::Frame::new()
        .fill(Palette::SIDE)
        .corner_radius(CornerRadius { nw: 0, ne: 0, sw: 6, se: 6 })
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let selected = st.selected_card();
            let usable = selected.is_some_and(Card::usable);
            ui.horizontal(|ui| {
                match (&st.use_error, selected) {
                    (Some(error), _) => {
                        ui.add(egui::Label::new(RichText::new(error).color(Palette::ERR)).truncate());
                    }
                    (None, Some(card)) if !card.usable() => {
                        let text = "The selected folder cannot start Bonsai";
                        ui.label(RichText::new(text).color(Palette::ERR));
                    }
                    (None, None) => {
                        ui.label(RichText::new("Select a folder or Browse...").color(Palette::TEXT_DIM));
                    }
                    (None, Some(_)) => {
                        let text = "Change it later in Bonsai > Change Bonsai folder...";
                        ui.label(RichText::new(text).color(Palette::TEXT_DIM));
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let accent = theme::accent(ui.ctx());
                    let use_button = egui::Button::new(RichText::new("Use this folder").strong().color(Color32::WHITE))
                        .fill(if usable { accent } else { Palette::WIDGET })
                        .min_size(egui::vec2(132.0, 26.0));
                    let tip = if usable { "Use the selected folder (Enter)" } else { "Select a usable folder first" };
                    let r = ui.add_enabled(usable, use_button).on_hover_text(tip).on_disabled_hover_text(tip);
                    if r.clicked() {
                        if let Some(card) = selected {
                            *outcome = Some(Outcome::Use(card.install.root.clone()));
                        }
                    }
                    let skip = ui
                        .add(egui::Button::new("Skip for now").min_size(egui::vec2(104.0, 26.0)))
                        .on_hover_text("Close without changing the folder (Esc)");
                    if skip.clicked() {
                        *outcome = Some(Outcome::Skip);
                    }
                });
            });
        });
}
