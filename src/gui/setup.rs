//! First-run dialog (also Bonsai > Change Bonsai folder...): three ways to get Bonsai running.
//!
//! - **Use model files on this PC** (the default without a Bonsai-demo folder): pick a `.gguf`;
//!   a `*mmproj*.gguf` next to it is found by itself (else an optional second picker). The files
//!   stay where they are. Without a known llama-server.exe the same page offers the server build
//!   for this PC (picked from nvidia-smi) with a progress bar; "Use this model" saves the paths.
//! - **Download Bonsai**: the server build + Bonsai 2 27B PTQ1_0 + its projector into the app's
//!   folder, with sizes and the free disk space.
//! - **Use a Bonsai-demo folder**: the installs found in the usual places or with Browse...,
//!   what each one contains, then use it as before - or import its files into the app.

use std::path::{Path, PathBuf};

use bh3::runtime::{self, Backend, ImportMode, RuntimeTask};
use bh3::server::{self, BonsaiInstall};
use bh3::service::JobId;
use eframe::egui::{
    self, text::LayoutJob, Align, Color32, CornerRadius, FontId, Id, Key, Layout, Margin, RichText, Sense, Stroke,
    StrokeKind, TextFormat, Ui,
};

use super::app::{AppState, ToastKind};
use super::models_ws::{self, TaskAction, Then};
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Width of the dialog content.
const WIDTH: f32 = 700.0;

/// The three ways.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Way {
    #[default]
    Files,
    Download,
    Folder,
}

/// "Use model files on this PC": the paths picked so far.
#[derive(Default)]
struct FilesForm {
    model: Option<PathBuf>,
    /// The projector (`None` = none: image input off).
    mmproj: Option<PathBuf>,
    /// The projector was found next to the model (not picked).
    mmproj_found: bool,
    /// llama-server.exe picked by hand (else the one the app knows).
    server: Option<PathBuf>,
    /// DLLs the shown server lacks, checked once per path.
    dll_check: Option<(PathBuf, Vec<String>)>,
    error: Option<String>,
}

/// Dialog state; the folders are scanned once each time the dialog opens.
#[derive(Default)]
pub struct SetupState {
    way: Way,
    cards: Vec<Card>,
    /// Root of the selected install.
    selected: Option<PathBuf>,
    /// Browse... picked a folder that is not a Bonsai install.
    browse_error: Option<String>,
    /// "Use this folder" failed.
    use_error: Option<String>,
    files: FilesForm,
    /// Import moves instead of copying.
    move_files: bool,
    /// Tasks started from this dialog (shown with progress here).
    tasks: Vec<JobId>,
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

impl SetupState {
    /// Fill the form from the settings and pick the page to show first.
    fn open(&mut self, app: &AppState) {
        let rt = &app.config.runtime;
        self.files = FilesForm {
            model: rt.model.clone(),
            mmproj: rt.mmproj.clone(),
            mmproj_found: false,
            server: rt.server_exe.clone(),
            ..FilesForm::default()
        };
        self.tasks.retain(|id| app.models.tasks.iter().any(|t| t.id == *id && t.running()));
        let usable_folder = self.cards.iter().any(Card::usable);
        self.way = if rt.is_active() || !usable_folder { Way::Files } else { Way::Folder };
        if let Ok(name) = std::env::var("BH3_SETUP_WAY") {
            self.way = match name.trim().to_ascii_lowercase().as_str() {
                "download" => Way::Download,
                "folder" => Way::Folder,
                _ => Way::Files,
            };
        }
    }

    /// A model was picked: remember it and look for its projector next to it.
    fn set_model(&mut self, path: PathBuf) {
        if !runtime::is_model_file(&path) {
            self.files.error = Some(format!("{} is not a model file (a vision projector?)", file_name(&path)));
            return;
        }
        self.files.mmproj = runtime::pair_mmproj(&path);
        self.files.mmproj_found = self.files.mmproj.is_some();
        self.files.model = Some(path);
        self.files.error = None;
    }
}

enum Outcome {
    /// Use this Bonsai-demo folder.
    Use(PathBuf),
    /// Use the model files of the form.
    UseFiles {
        model: PathBuf,
        mmproj: Option<PathBuf>,
        server: PathBuf,
    },
    /// Download the server build and / or the recommended model, then use them.
    Download {
        server: Option<Backend>,
        model: bool,
    },
    /// Copy / move a Bonsai-demo folder's files into the app, then use them.
    Import(PathBuf),
    Skip,
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    let mut st = std::mem::take(&mut app.setup);
    let pass = ctx.cumulative_pass_nr();
    if st.last_pass.is_none_or(|last| last + 1 < pass) {
        st.selected = None;
        st.rescan(app.config.bonsai_dir.as_deref());
        st.open(app);
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
        Some(Outcome::UseFiles { model, mmproj, server }) => {
            models_ws::use_files(app, Some((model, mmproj)), Some(server));
            app.show_setup = false;
            st.last_pass = None;
        }
        Some(Outcome::Download { server, model }) => {
            if let Some(backend) = server {
                st.tasks.push(models_ws::start(app, RuntimeTask::DownloadServer(backend), Then::UseServer));
            }
            if model {
                let task = RuntimeTask::DownloadModel(runtime::default_model().source());
                st.tasks.push(models_ws::start(app, task, Then::UseModel));
            }
        }
        Some(Outcome::Import(root)) => {
            let mode = if st.move_files { ImportMode::Move } else { ImportMode::Copy };
            st.tasks.push(models_ws::start(app, RuntimeTask::Import { dir: root, mode }, Then::UseAll));
        }
        Some(Outcome::Skip) => {
            app.show_setup = false;
            st.last_pass = None;
            if server::install_from_config(&app.config).is_none() && !app.models.any_running() {
                app.toast(
                    ToastKind::Info,
                    "Bonsai is not set up yet - do it any time in Bonsai > Change Bonsai folder...",
                );
            }
        }
        None => {}
    }
    app.setup = st;
}

fn dialog(ctx: &egui::Context, st: &mut SetupState, app: &mut AppState) -> Option<Outcome> {
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
                ui.add_space(10.0);
                way_picker(ui, &mut st.way);
                ui.add_space(10.0);
                let max_h = (screen_h - 360.0).clamp(160.0, 470.0);
                egui::ScrollArea::vertical()
                    .id_salt(("bh3-setup-page", st.way as u8))
                    .max_height(max_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| match st.way {
                        Way::Files => files_page(ui, st, app),
                        Way::Download => download_page(ui, st, app),
                        Way::Folder => folder_page(ui, st, app, &mut outcome),
                    });
            });
            footer(ui, st, app, &mut outcome);
        });

    if modal.is_top_modal && !modal.any_popup_open {
        let (esc, enter) =
            ctx.input_mut(|i| (i.consume_key(egui::Modifiers::NONE, Key::Escape), i.key_pressed(Key::Enter)));
        if esc {
            outcome = Some(Outcome::Skip);
        } else if enter && outcome.is_none() {
            outcome = main_action(st, app).filter(|_| ctx.memory(|m| m.focused().is_none()));
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
    let stop = match idle_minutes {
        0 => "can stop Bonsai after idle minutes (Settings) to free it".to_string(),
        1 => "stops Bonsai after 1 idle minute to free it".to_string(),
        m => format!("stops Bonsai after {m} idle minutes to free it"),
    };
    let mut job = LayoutJob::default();
    job.append("The app runs ", 0.0, normal.clone());
    job.append("Bonsai", 0.0, strong.clone());
    job.append(
        " itself: a model file (.gguf) and PrismML's llama-server. Choose where they come from - no \
         Bonsai-demo folder is needed. Bonsai and ComfyUI ",
        0.0,
        normal.clone(),
    );
    job.append("cannot share the 8 GB GPU", 0.0, strong);
    job.append(format!(", so the app {stop}.").as_str(), 0.0, normal);
    ui.label(job);
}

/// Three selectable cards: the ways to set Bonsai up.
fn way_picker(ui: &mut Ui, way: &mut Way) {
    let ways = [
        (Way::Files, icons::FOLDER, "Model files on this PC", "Your .gguf - used where it is, nothing is copied"),
        (Way::Download, "⬇", "Download Bonsai", "Recommended: the 27B model + server, about 6.7 GB"),
        (Way::Folder, icons::SERVER, "Bonsai-demo folder", "Start it as before, or import its files"),
    ];
    ui.columns(3, |cols| {
        for (col, (w, icon, title, detail)) in cols.iter_mut().zip(ways) {
            let selected = *way == w;
            let fill =
                if selected { Palette::PANEL_BODY.lerp_to_gamma(Palette::BLUE, 0.28) } else { Palette::PANEL_BODY };
            let stroke = if selected {
                Stroke::new(1.0_f32, Palette::BLUE_HOVER)
            } else {
                Stroke::new(1.0_f32, Palette::OUTLINE)
            };
            let shown = egui::Frame::new()
                .fill(fill)
                .stroke(stroke)
                .corner_radius(CornerRadius::same(5))
                .inner_margin(Margin::symmetric(10, 8))
                .show(col, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(50.0);
                    ui.horizontal(|ui| {
                        radio(ui, selected);
                        ui.label(RichText::new(icon).color(if selected { Color32::WHITE } else { Palette::TEXT_DIM }));
                        ui.add(egui::Label::new(RichText::new(title).strong().color(Palette::TEXT)).truncate());
                    });
                    ui.add(egui::Label::new(RichText::new(detail).small().color(Palette::TEXT_DIM)).wrap());
                });
            let rect = shown.response.rect;
            let response = col.interact(rect, Id::new(("bh3-setup-way", w as u8)), Sense::click());
            if response.hovered() && !selected {
                col.painter().rect_stroke(
                    rect,
                    CornerRadius::same(5),
                    Stroke::new(1.0_f32, Color32::from_gray(96)),
                    StrokeKind::Inside,
                );
            }
            if response.on_hover_text(detail).clicked() {
                *way = w;
            }
        }
    });
}

/// Read-only path in a text-field look, cut with "…" from the left part when too long.
fn path_box(ui: &mut Ui, text: &str, width: f32, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width.max(60.0), 22.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius::same(4),
            Palette::INPUT,
            Stroke::new(1.0_f32, Palette::OUTLINE),
            StrokeKind::Inside,
        );
        // A long path keeps its end (the file name): leading folders are dropped first.
        let room = rect.width() - 14.0;
        let parts: Vec<&str> = text.split('\\').collect();
        let mut shown = text.to_string();
        let mut skip = 0;
        while skip + 1 < parts.len() && painter.layout_no_wrap(shown.clone(), theme::mono(12.0), color).size().x > room
        {
            skip += 1;
            shown = format!("…\\{}", parts[skip..].join("\\"));
        }
        let mut job = LayoutJob::simple_singleline(shown, theme::mono(12.0), color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(room);
        let galley = painter.layout_job(job);
        painter.galley(egui::pos2(rect.left() + 7.0, rect.center().y - galley.size().y / 2.0), galley, color);
    }
    response.on_hover_text(text)
}

/// Path box filling the row, with `buttons` (label, tip) after it; returns the clicked index.
fn path_row(ui: &mut Ui, text: &str, color: Color32, buttons: &[(&str, &str)]) -> Option<usize> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        let spacing = ui.spacing().item_spacing.x;
        let widths: Vec<f32> = buttons
            .iter()
            .map(|(label, _)| {
                let galley =
                    ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.5), Color32::PLACEHOLDER);
                (galley.size().x + 2.0 * ui.spacing().button_padding.x).max(26.0).ceil()
            })
            .collect();
        let used: f32 = widths.iter().map(|w| w + spacing).sum();
        path_box(ui, text, ui.available_width() - used, color);
        for (i, ((label, tip), w)) in buttons.iter().zip(widths).enumerate() {
            if ui.add(egui::Button::new(*label).min_size(egui::vec2(w, 22.0))).on_hover_text(*tip).clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

fn small_note(ui: &mut Ui, icon: &str, text: &str, color: Color32) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(icon).small().color(color));
        ui.add(egui::Label::new(RichText::new(text).small().color(color)).wrap());
    });
}

/// The tasks this dialog started, with their progress and buttons.
fn dialog_tasks(ui: &mut Ui, st: &mut SetupState, app: &mut AppState, filter: impl Fn(&RuntimeTask) -> bool) {
    let views: Vec<_> =
        app.models.tasks.iter().filter(|t| st.tasks.contains(&t.id) && filter(&t.task)).cloned().collect();
    let mut actions: Vec<(JobId, TaskAction)> = Vec::new();
    for view in &views {
        ui.add_space(4.0);
        egui::Frame::new()
            .fill(Palette::INPUT.lerp_to_gamma(Palette::PANEL_BODY, 0.6))
            .corner_radius(CornerRadius::same(4))
            .inner_margin(Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.push_id(("bh3-setup-task", view.id), |ui| {
                    if let Some(a) = models_ws::task_row(ui, view) {
                        actions.push((view.id, a));
                    }
                });
            });
    }
    for (id, action) in actions {
        models_ws::task_action(app, id, action);
        if action == TaskAction::Resume {
            if let Some(newest) = app.models.tasks.last() {
                let new_id = newest.id;
                if let Some(slot) = st.tasks.iter_mut().find(|t| **t == id) {
                    *slot = new_id;
                }
            }
        }
    }
}

// --- Use model files on this PC ---------------------------------------------------------------

fn files_page(ui: &mut Ui, st: &mut SetupState, app: &mut AppState) {
    small_note(
        ui,
        icons::INFO,
        "The files stay where they are - nothing is copied or moved; the app only remembers their paths \
         (e.g. straight from a Bonsai-demo folder or another drive).",
        Palette::INFO,
    );
    ui.add_space(6.0);
    widgets::section_label(ui, "Model (.gguf)");
    let model_text = st.files.model.as_ref().map_or("No model chosen".to_string(), |p| p.display().to_string());
    let color = if st.files.model.is_some() { Palette::TEXT } else { Palette::TEXT_DIM };
    if path_row(ui, &model_text, color, &[(&format!("{} Browse...", icons::FOLDER), "Pick the model's .gguf file")])
        .is_some()
    {
        let start = st.files.model.clone();
        if let Some(path) = util::pick_file("Choose the model (.gguf)", "GGUF model", &["gguf"], start.as_deref()) {
            st.set_model(path);
        }
    }
    if let Some(model) = &st.files.model {
        let size = std::fs::metadata(model).map(|m| runtime::fmt_bytes(m.len()));
        match size {
            Ok(size) => {
                let known =
                    runtime::catalog_by_file(&file_name(model)).map(|m| format!(" · {}", m.title)).unwrap_or_default();
                small_note(ui, icons::CHECK, &format!("{size}{known}"), Palette::OK);
            }
            Err(_) => small_note(ui, icons::CROSS, "The file does not exist", Palette::ERR),
        }
    }
    if let Some(error) = &st.files.error {
        small_note(ui, icons::CROSS, error, Palette::ERR);
    }

    ui.add_space(6.0);
    widgets::section_label(ui, "Vision projector (optional)");
    let mm_text = st.files.mmproj.as_ref().map_or("None - image input off".to_string(), |p| p.display().to_string());
    let mm_color = if st.files.mmproj.is_some() { Palette::TEXT } else { Palette::TEXT_DIM };
    let browse = format!("{} Browse...", icons::FOLDER);
    let buttons: Vec<(&str, &str)> = if st.files.mmproj.is_some() {
        vec![(browse.as_str(), "Pick a *mmproj*.gguf"), ("✖", "No projector: image input off")]
    } else {
        vec![(browse.as_str(), "Pick a *mmproj*.gguf")]
    };
    match path_row(ui, &mm_text, mm_color, &buttons) {
        Some(0) => {
            let start = st.files.mmproj.clone().or_else(|| st.files.model.clone());
            if let Some(path) = util::pick_file(
                "Choose the vision projector (*mmproj*.gguf)",
                "GGUF projector",
                &["gguf"],
                start.as_deref(),
            ) {
                st.files.mmproj = Some(path);
                st.files.mmproj_found = false;
            }
        }
        Some(_) => {
            st.files.mmproj = None;
            st.files.mmproj_found = false;
        }
        None => {}
    }
    match (&st.files.model, &st.files.mmproj) {
        (Some(_), Some(_)) if st.files.mmproj_found => {
            small_note(ui, icons::CHECK, "Found next to the model: pictures can be sent to Bonsai", Palette::OK)
        }
        (Some(_), None) => small_note(
            ui,
            icons::WARN,
            "No *mmproj*.gguf next to the model - image input stays off unless you pick one (text works without it)",
            Palette::WARN,
        ),
        _ => {}
    }

    ui.add_space(6.0);
    widgets::section_label(ui, "Server (llama-server.exe)");
    server_block(ui, st, app);
}

/// The server a "Use this model" would save: picked by hand, else the one the app knows.
fn files_server(st: &SetupState, app: &mut AppState) -> Option<PathBuf> {
    st.files.server.clone().filter(|p| p.is_file()).or_else(|| models_ws::known_server(app))
}

/// DLLs `exe` lacks (checked once per path).
fn missing_dlls(st: &mut SetupState, exe: &Path) -> Vec<String> {
    match &st.files.dll_check {
        Some((checked, missing)) if checked == exe => missing.clone(),
        _ => {
            let missing = runtime::missing_system_dlls(exe);
            st.files.dll_check = Some((exe.to_path_buf(), missing.clone()));
            missing
        }
    }
}

fn server_block(ui: &mut Ui, st: &mut SetupState, app: &mut AppState) {
    let browse = format!("{} Browse...", icons::FOLDER);
    match files_server(st, app) {
        Some(exe) => {
            let kind = runtime::build_kind(&exe).unwrap_or("?").to_uppercase();
            if path_row(ui, &exe.display().to_string(), Palette::TEXT, &[(&browse, "Use another llama-server.exe")])
                .is_some()
            {
                if let Some(path) = util::pick_file("Choose llama-server.exe", "llama-server", &["exe"], Some(&exe)) {
                    st.files.server = Some(path);
                }
            }
            let source = if st.files.server.as_deref().is_some_and(|s| runtime::same_file(s, &exe)) {
                "chosen by you"
            } else if runtime::is_inside(&exe, &runtime::runtime_dir(&app.config)) {
                "downloaded by the app"
            } else {
                "from the Bonsai-demo folder"
            };
            let missing = missing_dlls(st, &exe);
            if missing.is_empty() {
                small_note(ui, icons::CHECK, &format!("{kind} build, {source}"), Palette::OK);
            } else {
                models_ws::vc_redist_note(ui, &missing);
                if ui
                    .button(format!("{} Check again", icons::RESTART))
                    .on_hover_text("After installing the Visual C++ runtime")
                    .clicked()
                {
                    st.files.dll_check = None;
                }
            }
        }
        None => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::CROSS).color(Palette::ERR));
                ui.label(RichText::new("Not installed").color(Palette::ERR));
                ui.label(
                    RichText::new("- Bonsai 2 needs PrismML's llama-server build (stock llama.cpp cannot load it)")
                        .small()
                        .color(Palette::TEXT_DIM),
                );
            });
            let choice = models_ws::backend_choice(app);
            let Some(choice) = choice else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Checking the GPU (nvidia-smi)...").color(Palette::TEXT_DIM));
                });
                return;
            };
            let backend = app.models.chosen_backend.unwrap_or(choice.backend);
            let color =
                if choice.cuda.is_some() && choice.backend.is_gpu() { Palette::TEXT_DIM } else { Palette::WARN };
            small_note(ui, if color == Palette::WARN { icons::WARN } else { icons::INFO }, &choice.reason, color);
            let running =
                models_ws::server_task(app, backend).filter(|t| st.tasks.contains(&t.id)).is_some_and(|t| t.running());
            ui.horizontal(|ui| {
                let extra =
                    if matches!(backend, Backend::Cuda124 | Backend::Cuda133) { " incl. CUDA runtime" } else { "" };
                let label = format!(
                    "⬇ Download server build ({}, {}{extra})",
                    backend.label(),
                    runtime::fmt_bytes(backend.download_bytes())
                );
                let button = egui::Button::new(RichText::new(label).color(Color32::WHITE))
                    .fill(Palette::BLUE)
                    .min_size(egui::vec2(0.0, 24.0));
                let r = ui.add_enabled(!running, button).on_hover_text(format!(
                    "llama-server.exe and its DLLs from PrismML's llama.cpp release {} into {}",
                    runtime::RELEASE_TAG,
                    runtime::bin_dir(&app.config).display()
                ));
                if r.clicked() {
                    let id = models_ws::start(app, RuntimeTask::DownloadServer(backend), Then::Keep);
                    st.tasks.push(id);
                }
                let mut pick = backend;
                egui::ComboBox::from_id_salt("bh3-setup-backend").selected_text(backend.label()).width(96.0).show_ui(
                    ui,
                    |ui| {
                        for b in Backend::ALL {
                            let label = if b == choice.backend {
                                format!("{} (for this PC)", b.label())
                            } else {
                                b.label().to_string()
                            };
                            ui.selectable_value(&mut pick, b, label);
                        }
                    },
                );
                if pick != backend {
                    app.models.chosen_backend = Some(pick);
                }
            });
            dialog_tasks(ui, st, app, |t| matches!(t, RuntimeTask::DownloadServer(_)));
            ui.horizontal(|ui| {
                ui.label(RichText::new("Already have one?").small().color(Palette::TEXT_DIM));
                if ui
                    .small_button(browse.as_str())
                    .on_hover_text("Pick an existing llama-server.exe (PrismML's build)")
                    .clicked()
                {
                    if let Some(path) =
                        util::pick_file("Choose llama-server.exe", "llama-server", &["exe"], st.files.model.as_deref())
                    {
                        st.files.server = Some(path);
                    }
                }
            });
        }
    }
}

// --- Download Bonsai ----------------------------------------------------------------------

/// What "Download and use" would fetch: the server build when none is in the app, the model when
/// it is not downloaded yet.
fn download_plan(app: &mut AppState) -> (Option<Backend>, bool, u64) {
    let backend = models_ws::wanted_backend(app);
    let has_build = !runtime::installed_builds(&app.config).is_empty();
    let server = backend.filter(|_| !has_build);
    let model = runtime::default_model();
    let path = model.source().model_path(&runtime::models_dir(&app.config));
    let need_model = !path.is_file();
    let bytes = server.map_or(0, Backend::download_bytes) + if need_model { model.total_bytes() } else { 0 };
    (server, need_model, bytes)
}

fn download_page(ui: &mut Ui, st: &mut SetupState, app: &mut AppState) {
    ui.add(
        egui::Label::new(
            RichText::new("Downloads the server build and Bonsai 2 27B PTQ1_0 with its vision projector into the app's folder, then uses them.")
                .color(Palette::TEXT_DIM),
        )
        .wrap(),
    );
    ui.add_space(6.0);
    let choice = models_ws::backend_choice(app);
    let backend = models_ws::wanted_backend(app);
    let builds = runtime::installed_builds(&app.config);
    let model = runtime::default_model();
    let models_dir = runtime::models_dir(&app.config);
    let model_here = model.source().model_path(&models_dir).is_file();
    let (_, _, bytes) = download_plan(app);
    egui::Grid::new("bh3-setup-download").num_columns(3).spacing([12.0, 5.0]).show(ui, |ui| {
        ui.label(RichText::new("Server build").color(Palette::TEXT_DIM));
        match (&builds.first(), backend) {
            (Some(b), _) => {
                ui.label(format!("{} · {}", b.backend, b.tag));
                widgets::badge(ui, "already here", Palette::OK);
            }
            (None, Some(backend)) => {
                ui.horizontal(|ui| {
                    let mut pick = backend;
                    egui::ComboBox::from_id_salt("bh3-setup-dl-backend")
                        .selected_text(backend.label())
                        .width(96.0)
                        .show_ui(ui, |ui| {
                            for b in Backend::ALL {
                                let recommended = choice.as_ref().is_some_and(|c| c.backend == b);
                                let label = if recommended {
                                    format!("{} (for this PC)", b.label())
                                } else {
                                    b.label().to_string()
                                };
                                ui.selectable_value(&mut pick, b, label);
                            }
                        });
                    if pick != backend {
                        app.models.chosen_backend = Some(pick);
                    }
                    ui.label(
                        RichText::new(format!("PrismML llama.cpp {}", runtime::RELEASE_TAG))
                            .small()
                            .color(Palette::TEXT_DIM),
                    );
                });
                ui.label(runtime::fmt_bytes(backend.download_bytes()));
            }
            (None, None) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Checking the GPU...").color(Palette::TEXT_DIM));
                });
                ui.label("");
            }
        }
        ui.end_row();
        ui.label(RichText::new("Model").color(Palette::TEXT_DIM));
        ui.label(model.model.name).on_hover_text(model.note);
        if model_here {
            widgets::badge(ui, "already here", Palette::OK);
        } else {
            ui.label(runtime::fmt_bytes(model.model.bytes));
        }
        ui.end_row();
        if let Some(mmproj) = model.mmproj {
            ui.label(RichText::new("Vision projector").color(Palette::TEXT_DIM));
            ui.label(mmproj.name);
            ui.label(runtime::fmt_bytes(mmproj.bytes));
            ui.end_row();
        }
        ui.label(RichText::new("Total").color(Palette::TEXT_DIM));
        let free = runtime::free_space(&models_dir);
        let enough = free.is_none_or(|f| f > bytes + 256 * 1024 * 1024);
        let free_text =
            free.map_or("free space unknown".to_string(), |f| format!("{} free on that drive", runtime::fmt_bytes(f)));
        ui.label(RichText::new(free_text).color(if enough { Palette::TEXT_DIM } else { Palette::ERR }));
        ui.label(RichText::new(runtime::fmt_bytes(bytes)).strong());
        ui.end_row();
    });
    if let Some(c) = &choice {
        let color = if c.backend.is_gpu() { Palette::TEXT_DIM } else { Palette::WARN };
        small_note(ui, if c.backend.is_gpu() { icons::INFO } else { icons::WARN }, &c.reason, color);
    }
    ui.add_space(6.0);
    widgets::section_label(ui, "Models folder");
    let change = format!("{} Change...", icons::FOLDER);
    let reset = "Default";
    let mut buttons = vec![(change.as_str(), "Store models elsewhere, e.g. on another drive")];
    if app.config.runtime.models_dir.is_some() {
        buttons.push((reset, "Back to the app's folder"));
    }
    match path_row(ui, &models_dir.display().to_string(), Palette::TEXT, &buttons) {
        Some(0) => {
            if let Some(dir) = util::pick_folder("Choose the folder for models", Some(&models_dir)) {
                app.config.runtime.models_dir = Some(dir);
                app.save_config();
            }
        }
        Some(_) => {
            app.config.runtime.models_dir = None;
            app.save_config();
        }
        None => {}
    }
    small_note(
        ui,
        icons::INFO,
        "A cancelled or broken download continues where it stopped next time.",
        Palette::TEXT_DIM,
    );
    dialog_tasks(ui, st, app, |t| matches!(t, RuntimeTask::DownloadServer(_) | RuntimeTask::DownloadModel(_)));
}

// --- Use a Bonsai-demo folder -------------------------------------------------------------

fn folder_page(ui: &mut Ui, st: &mut SetupState, app: &mut AppState, outcome: &mut Option<Outcome>) {
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Bonsai-demo folders found");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let rescan = ui
                .add(egui::Button::new(format!("{} Rescan", icons::RESTART)).min_size(egui::vec2(0.0, 22.0)))
                .on_hover_text(r"Look again in D:\, C:\, E:\, your user folder, Documents and Desktop");
            if rescan.clicked() {
                st.rescan(app.config.bonsai_dir.as_deref());
            }
        });
    });
    ui.add_space(2.0);
    cards(ui, st, app, outcome);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let browse = ui
            .add(egui::Button::new(format!("{} Browse...", icons::FOLDER)).min_size(egui::vec2(96.0, 24.0)))
            .on_hover_text("Choose the Bonsai-demo folder yourself");
        if browse.clicked() {
            st.browse(app.config.bonsai_dir.as_deref());
        }
        ui.label(RichText::new("Pick the folder that contains start_bonsai.bat").color(Palette::TEXT_DIM));
    });
    ui.add_space(8.0);
    widgets::thin_separator(ui);
    ui.add_space(6.0);
    let selected = st.selected_card().filter(|c| c.usable()).map(|c| c.install.clone());
    let bytes: u64 = selected
        .as_ref()
        .map(|i| {
            i.primary_model()
                .into_iter()
                .chain(i.mmproj.as_ref())
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0);
    ui.horizontal(|ui| {
        let running = app.models.tasks.iter().any(|t| t.running() && matches!(t.task, RuntimeTask::Import { .. }));
        let r = ui
            .add_enabled(
                selected.is_some() && !running,
                egui::Button::new(format!("⬇ Import into the app ({})", runtime::fmt_bytes(bytes))),
            )
            .on_hover_text("Copy llama-server, the model and its projector into the app's folder, then use them")
            .on_disabled_hover_text(if running { "An import is running" } else { "Select a usable folder first" });
        if r.clicked() {
            if let Some(install) = &selected {
                *outcome = Some(Outcome::Import(install.root.clone()));
            }
        }
        ui.checkbox(&mut st.move_files, "Move instead of copy")
            .on_hover_text("Moves the files out of the Bonsai-demo folder (its launcher can no longer start them)");
    });
    small_note(
        ui,
        icons::INFO,
        "Optional: \"Use this folder\" needs no copy. Importing makes the app independent of the folder.",
        Palette::TEXT_DIM,
    );
    dialog_tasks(ui, st, app, |t| matches!(t, RuntimeTask::Import { .. }));
}

/// What Enter / the main button does on the current page (`None`: not possible now).
fn main_action(st: &mut SetupState, app: &mut AppState) -> Option<Outcome> {
    match st.way {
        Way::Files => {
            let model = st.files.model.clone().filter(|m| m.is_file())?;
            let server = files_server(st, app)?;
            if !missing_dlls(st, &server).is_empty() {
                return None;
            }
            Some(Outcome::UseFiles { model, mmproj: st.files.mmproj.clone(), server })
        }
        Way::Download => {
            if app.models.tasks.iter().any(|t| st.tasks.contains(&t.id) && t.running()) {
                return None;
            }
            let (server, model, _) = download_plan(app);
            if server.is_none() && !model {
                // Everything is here: use it.
                let src = runtime::default_model().source();
                let dir = runtime::models_dir(&app.config);
                let server = models_ws::known_server(app)?;
                return Some(Outcome::UseFiles { model: src.model_path(&dir), mmproj: src.mmproj_path(&dir), server });
            }
            if server.is_none() && runtime::installed_builds(&app.config).is_empty() {
                return None; // the GPU check has not answered yet
            }
            Some(Outcome::Download { server, model })
        }
        Way::Folder => st.selected_card().filter(|c| c.usable()).map(|c| Outcome::Use(c.install.root.clone())),
    }
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

fn footer(ui: &mut Ui, st: &mut SetupState, app: &mut AppState, outcome: &mut Option<Outcome>) {
    let (line, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(line, 0.0, Palette::OUTLINE);
    let action = main_action(st, app);
    let downloading = app.models.tasks.iter().any(|t| st.tasks.contains(&t.id) && t.running());
    let (status, color, label, tip) = match st.way {
        Way::Files => {
            let server = files_server(st, app);
            let (status, color) = if st.files.model.as_ref().is_none_or(|m| !m.is_file()) {
                ("Choose the model file (.gguf)".to_string(), Palette::TEXT_DIM)
            } else if server.is_none() {
                ("Download or choose the server first".to_string(), Palette::WARN)
            } else if action.is_none() {
                ("Install the Visual C++ runtime first".to_string(), Palette::ERR)
            } else {
                ("Bonsai starts from these files when a prompt needs it".to_string(), Palette::TEXT_DIM)
            };
            (status, color, "Use this model", "Save these paths and use them (Enter)")
        }
        Way::Download => {
            let status = if downloading {
                "Downloading - you can close this; it goes on in Bonsai > Models".to_string()
            } else {
                "Change it later in Bonsai > Models".to_string()
            };
            (status, Palette::TEXT_DIM, "Download and use", "Download what is missing, then use it (Enter)")
        }
        Way::Folder => {
            let selected = st.selected_card();
            let (status, color) = match (&st.use_error, selected) {
                (Some(error), _) => (error.clone(), Palette::ERR),
                (None, Some(card)) if !card.usable() => {
                    ("The selected folder cannot start Bonsai".to_string(), Palette::ERR)
                }
                (None, None) => ("Select a folder or Browse...".to_string(), Palette::TEXT_DIM),
                (None, Some(_)) => {
                    ("Change it later in Bonsai > Change Bonsai folder...".to_string(), Palette::TEXT_DIM)
                }
            };
            (status, color, "Use this folder", "Start Bonsai from the selected folder's launcher (Enter)")
        }
    };
    egui::Frame::new()
        .fill(Palette::SIDE)
        .corner_radius(CornerRadius { nw: 0, ne: 0, sw: 6, se: 6 })
        .inner_margin(Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let enabled = action.is_some();
                    let accent = theme::accent(ui.ctx());
                    let main = egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                        .fill(if enabled { accent } else { Palette::WIDGET })
                        .min_size(egui::vec2(132.0, 26.0));
                    let r = ui.add_enabled(enabled, main).on_hover_text(tip).on_disabled_hover_text(&status);
                    if r.clicked() {
                        *outcome = action;
                    }
                    let close_label = if downloading || app.models.any_running() { "Close" } else { "Skip for now" };
                    let skip = ui
                        .add(egui::Button::new(close_label).min_size(egui::vec2(104.0, 26.0)))
                        .on_hover_text("Close (Esc); downloads go on in the background");
                    if skip.clicked() {
                        *outcome = Some(Outcome::Skip);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(&status).color(color)).truncate());
                    });
                });
            });
        });
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
