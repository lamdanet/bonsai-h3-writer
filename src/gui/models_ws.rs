//! Models area of the Bonsai workspace and the downloads / imports shared with the setup dialog.
//!
//! The area (switched with Log in the bottom area's header) shows what Bonsai starts from (the
//! app's own runtime or the Bonsai-demo folder), running and finished tasks with progress bars
//! (MB/s, ETA, Cancel, Resume), the server build for this PC, the models on this PC (Set active,
//! Delete), the catalog and a form for any Hugging Face repo / file. Model files chosen with
//! "Use a .gguf on this PC" stay where they are; only their paths are saved.
//!
//! Tasks run in the service ([`Command::RunTask`]); [`on_event`] follows their events and applies
//! the result when the task was started with a [`Then`] that says so.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bh3::runtime::{
    self, Backend, BackendChoice, InstalledBuild, InstalledModel, ModelSource, RuntimeTask, TaskProgress,
};
use bh3::server::ServerState;
use bh3::service::{Command, Event, JobId};
use eframe::egui::{self, Align, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, Ui};

use super::app::{AppState, ToastKind};
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Re-read the models folder this often (seconds); also after every task.
const SCAN_EVERY: f64 = 5.0;
/// Width of the dim labels of the "In use" card.
const LABEL_W: f32 = 64.0;

/// What to do with a finished task's files.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Then {
    /// Only download / import.
    Keep,
    /// Use the downloaded model and its projector.
    UseModel,
    /// Use the downloaded server build.
    UseServer,
    /// Use everything the task produced (an import).
    UseAll,
}

/// Where a task is.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskState {
    Running,
    Done(String),
    Failed(String),
    Cancelled,
}

/// A task as the UI shows it.
#[derive(Clone, Debug)]
pub struct TaskView {
    pub id: JobId,
    pub task: RuntimeTask,
    pub then: Then,
    pub progress: Option<TaskProgress>,
    pub state: TaskState,
    /// Cancel was pressed (the task ends at the next chunk).
    pub cancelling: bool,
}

impl TaskView {
    pub fn running(&self) -> bool {
        self.state == TaskState::Running
    }
}

/// Button pressed on a task row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskAction {
    Cancel,
    /// Run the same task again (a download continues from its `.part`).
    Resume,
    Dismiss,
}

/// What was read from disk (refreshed every few seconds and after tasks).
#[derive(Clone, Default)]
struct Scan {
    models: Vec<InstalledModel>,
    builds: Vec<InstalledBuild>,
    models_dir: PathBuf,
    free: Option<u64>,
    at: f64,
}

pub struct ModelsState {
    pub tasks: Vec<TaskView>,
    scan: Option<Scan>,
    /// Re-read the folder on the next frame.
    dirty: bool,
    /// nvidia-smi runs on a helper thread once.
    backend: Arc<Mutex<Option<BackendChoice>>>,
    detecting: bool,
    /// Build chosen for download (`None` = the recommended one).
    pub chosen_backend: Option<Backend>,
    hf_repo: String,
    hf_file: String,
    hf_mmproj: String,
    /// "Delete ...?" is open for this model.
    confirm_delete: Option<InstalledModel>,
    /// A new model / server was chosen while Bonsai runs: offer a restart.
    pub restart_hint: bool,
    /// `BH3_MODELS_SCROLL` (screenshots): scroll the area this far down.
    scroll_env: Option<f32>,
}

impl Default for ModelsState {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            scan: None,
            dirty: true,
            backend: Arc::new(Mutex::new(None)),
            detecting: false,
            chosen_backend: None,
            hf_repo: String::new(),
            hf_file: String::new(),
            hf_mmproj: String::new(),
            confirm_delete: None,
            restart_hint: false,
            scroll_env: std::env::var("BH3_MODELS_SCROLL").ok().and_then(|v| v.trim().parse().ok()),
        }
    }
}

impl ModelsState {
    /// A download or import runs.
    pub fn any_running(&self) -> bool {
        self.tasks.iter().any(TaskView::running)
    }
}

// ---------------------------------------------------------------------------------------------
// Tasks

/// Start a task in the service; returns its id.
pub fn start(app: &mut AppState, task: RuntimeTask, then: Then) -> JobId {
    let id = app.new_job_id();
    app.models.tasks.retain(|t| t.running() || t.task != task);
    app.models.tasks.push(TaskView {
        id,
        task: task.clone(),
        then,
        progress: None,
        state: TaskState::Running,
        cancelling: false,
    });
    app.send(Command::RunTask { id, task });
    id
}

/// Apply a button of a task row.
pub fn task_action(app: &mut AppState, id: JobId, action: TaskAction) {
    match action {
        TaskAction::Cancel => {
            if let Some(view) = app.models.tasks.iter_mut().find(|t| t.id == id) {
                view.cancelling = true;
            }
            app.send(Command::CancelTask(id));
        }
        TaskAction::Resume => {
            if let Some(view) = app.models.tasks.iter().find(|t| t.id == id).cloned() {
                start(app, view.task, view.then);
            }
        }
        TaskAction::Dismiss => app.models.tasks.retain(|t| t.id != id),
    }
}

/// Follow the service's task events.
pub fn on_event(app: &mut AppState, ev: &Event) {
    match ev {
        Event::TaskProgress { id, progress } => {
            if let Some(view) = app.models.tasks.iter_mut().find(|t| t.id == *id) {
                view.progress = Some(progress.clone());
            }
        }
        Event::TaskDone { id, outcome } => {
            app.models.dirty = true;
            let Some(view) = app.models.tasks.iter_mut().find(|t| t.id == *id) else { return };
            view.state = TaskState::Done(outcome.message.clone());
            let then = view.then;
            let (model, mmproj, server) = (outcome.model.clone(), outcome.mmproj.clone(), outcome.server_exe.clone());
            for note in &outcome.notes {
                let kind = if note.contains("Visual C++") { ToastKind::Error } else { ToastKind::Warn };
                app.toast(kind, note.clone());
            }
            match then {
                Then::Keep => app.toast(ToastKind::Ok, outcome.message.clone()),
                Then::UseModel => use_files(app, model.map(|m| (m, mmproj)), None),
                Then::UseServer => use_files(app, None, server),
                Then::UseAll => use_files(app, model.map(|m| (m, mmproj)), server),
            }
        }
        Event::TaskFailed { id, error } => {
            app.models.dirty = true;
            if let Some(view) = app.models.tasks.iter_mut().find(|t| t.id == *id) {
                view.state = TaskState::Failed(error.clone());
            } else {
                app.toast(ToastKind::Error, error.clone());
            }
        }
        Event::TaskCancelled(id) => {
            app.models.dirty = true;
            if let Some(view) = app.models.tasks.iter_mut().find(|t| t.id == *id) {
                view.state = TaskState::Cancelled;
            }
        }
        _ => {}
    }
}

/// Use these files from the next start: `model` with its projector (`None` = no image input)
/// and / or a server build. The files stay where they are.
pub fn use_files(app: &mut AppState, model: Option<(PathBuf, Option<PathBuf>)>, server: Option<PathBuf>) {
    let mut what = Vec::new();
    if let Some((model, mmproj)) = model {
        what.push(file_name(&model));
        app.config.runtime.model = Some(model);
        app.config.runtime.mmproj = mmproj;
    }
    if let Some(server) = server {
        what.push(format!("server {}", short_path(&server, 2)));
        app.config.runtime.server_exe = Some(server);
    }
    let install = app.apply_runtime();
    app.models.dirty = true;
    let running = matches!(app.status.state, ServerState::Ready | ServerState::Loading | ServerState::Starting);
    let problems = install.as_ref().map(|i| i.problems()).unwrap_or_default();
    if running {
        app.models.restart_hint = true;
    }
    let when = if running { "from the next start (Restart applies it now)" } else { "when Bonsai starts" };
    if problems.is_empty() {
        app.toast(ToastKind::Ok, format!("Using {} {when}", what.join(" + ")));
    } else {
        app.toast(ToastKind::Warn, format!("Using {} - still missing: {}", what.join(" + "), problems.join("; ")));
    }
}

/// The build for this PC (nvidia-smi on a helper thread; `None` until it answered).
pub fn backend_choice(app: &mut AppState) -> Option<BackendChoice> {
    let st = &mut app.models;
    let found = st.backend.lock().ok().and_then(|g| g.clone());
    if found.is_none() && !st.detecting {
        st.detecting = true;
        let slot = Arc::clone(&st.backend);
        let _ = std::thread::Builder::new().name("bh3-backend".into()).spawn(move || {
            let choice = runtime::detect_backend();
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(choice);
            }
        });
    }
    found
}

/// The build to download: the chosen one, else the recommended one.
pub fn wanted_backend(app: &mut AppState) -> Option<Backend> {
    let chosen = app.models.chosen_backend;
    chosen.or_else(|| backend_choice(app).map(|c| c.backend))
}

/// The llama-server.exe a runtime start would use (configured, else a build in the app, else
/// the Bonsai-demo folder's).
pub fn known_server(app: &mut AppState) -> Option<PathBuf> {
    let scan = scan(app);
    app.config
        .runtime
        .server_exe
        .clone()
        .filter(|p| p.is_file())
        .or_else(|| scan.builds.first().map(|b| b.exe.clone()))
        .or_else(|| app.status.install.as_ref().filter(|i| !i.runtime).and_then(|i| i.server_exe.clone()))
}

/// The task that downloads `backend`'s build, if one is shown.
pub fn server_task(app: &AppState, backend: Backend) -> Option<&TaskView> {
    app.models.tasks.iter().rev().find(|t| t.task == RuntimeTask::DownloadServer(backend))
}

fn scan(app: &mut AppState) -> Scan {
    let now = app.time;
    let stale = app.models.scan.as_ref().is_none_or(|s| now - s.at > SCAN_EVERY);
    if app.models.dirty || stale {
        let cfg = &app.config;
        let models_dir = runtime::models_dir(cfg);
        let mut models = runtime::scan_models(&models_dir);
        if let Some(active) = cfg.runtime.model.as_ref().filter(|m| m.is_file()) {
            if !models.iter().any(|m| runtime::same_file(&m.path, active)) {
                models.insert(0, runtime::describe_model(active, Some(&models_dir)));
            }
        }
        let free = runtime::free_space(&models_dir);
        app.models.scan = Some(Scan { models, builds: runtime::installed_builds(cfg), models_dir, free, at: now });
        app.models.dirty = false;
    }
    app.models.scan.clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Shared widgets

/// Flat progress bar with centred text (Blender style).
pub fn progress_bar(ui: &mut Ui, fraction: f32, color: Color32, text: &str) -> egui::Response {
    let width = ui.available_width().max(60.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 18.0), Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(3), Palette::INPUT);
        let filled =
            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * fraction.clamp(0.0, 1.0), rect.height()));
        if filled.width() >= 1.0 {
            painter.rect_filled(filled, CornerRadius::same(3), color.gamma_multiply(0.85));
        }
        let font = FontId::proportional(12.0);
        painter.text(
            rect.center() + egui::vec2(1.0, 1.0),
            egui::Align2::CENTER_CENTER,
            text,
            font.clone(),
            Color32::from_black_alpha(170),
        );
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, font, Color32::WHITE);
    }
    response
}

/// One task: title, progress bar with speed and ETA, and Cancel / Resume / Dismiss.
pub fn task_row(ui: &mut Ui, view: &TaskView) -> Option<TaskAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        let (icon, color) = match &view.state {
            TaskState::Running => ("⏬", Palette::INFO),
            TaskState::Done(_) => (icons::CHECK, Palette::OK),
            TaskState::Failed(_) => (icons::CROSS, Palette::ERR),
            TaskState::Cancelled => ("⏸", Palette::WARN),
        };
        ui.label(RichText::new(icon).color(color));
        ui.add(egui::Label::new(RichText::new(view.task.title()).strong()).truncate());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| match &view.state {
            TaskState::Running => {
                let cancel = ui.add_enabled(!view.cancelling, egui::Button::new("Cancel"));
                if cancel.on_hover_text("Stop; a download keeps what it has and continues next time").clicked() {
                    action = Some(TaskAction::Cancel);
                }
            }
            TaskState::Done(_) => {
                if ui.small_button("✖").on_hover_text("Remove from the list").clicked() {
                    action = Some(TaskAction::Dismiss);
                }
            }
            TaskState::Failed(_) | TaskState::Cancelled => {
                if ui.small_button("✖").on_hover_text("Remove from the list").clicked() {
                    action = Some(TaskAction::Dismiss);
                }
                let label = if matches!(view.task, RuntimeTask::Import { .. }) { "Try again" } else { "Resume" };
                if ui.button(format!("{} {label}", icons::PLAY)).on_hover_text("Continue where it stopped").clicked() {
                    action = Some(TaskAction::Resume);
                }
            }
        });
    });
    match &view.state {
        TaskState::Running => {
            let (fraction, text) = match &view.progress {
                Some(p) => {
                    let files = if p.files > 1 { format!(" ({}/{})", p.file_index, p.files) } else { String::new() };
                    let text = if view.cancelling {
                        "Cancelling...".to_string()
                    } else {
                        format!("{} {}{files}", p.phase, p.line())
                    };
                    (p.fraction(), text)
                }
                None => (0.0, "Starting...".to_string()),
            };
            progress_bar(ui, fraction, Palette::BLUE, &text)
                .on_hover_text(view.progress.as_ref().map(|p| p.file.clone()).unwrap_or_default());
        }
        TaskState::Done(message) => {
            ui.add(egui::Label::new(RichText::new(message).small().color(Palette::TEXT_DIM)).wrap());
        }
        TaskState::Failed(error) => {
            ui.add(egui::Label::new(RichText::new(error).small().color(Palette::ERR)).wrap());
        }
        TaskState::Cancelled => {
            let text = match &view.progress {
                Some(p) if p.total > 0 => format!("Cancelled at {} - Resume continues from there", p.line()),
                _ => "Cancelled - Resume continues from there".to_string(),
            };
            ui.label(RichText::new(text).small().color(Palette::WARN));
        }
    }
    action
}

/// Coloured box with an icon and wrapped text.
pub fn callout(ui: &mut Ui, color: Color32, icon: &str, text: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon).color(color));
                ui.add(egui::Label::new(text).wrap());
            });
        });
}

/// "The Visual C++ runtime is missing" with the download link.
pub fn vc_redist_note(ui: &mut Ui, missing: &[String]) {
    let Some(message) = runtime::missing_dlls_message(missing) else { return };
    callout(ui, Palette::ERR, icons::CROSS, &message);
    if message.contains("Visual C++") {
        ui.hyperlink_to(
            format!("{} Download the Visual C++ runtime (Microsoft)", icons::FOLDER),
            runtime::VC_REDIST_URL,
        );
    }
}

fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(Palette::PANEL_BODY)
        .stroke(Stroke::new(1.0_f32, Palette::OUTLINE))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(8.0);
}

// ---------------------------------------------------------------------------------------------
// The area

/// What the user asked for (applied after drawing).
enum Action {
    Task(JobId, TaskAction),
    Start(RuntimeTask, Then),
    UseModel(PathBuf, Option<PathBuf>),
    UseServer(PathBuf),
    BrowseModel,
    BrowseServer,
    UseFolder,
    Delete(InstalledModel),
    Open(PathBuf),
    Restart,
    ChooseBackend(Backend),
}

/// Header strip contents on the right: the models folder and its free space.
pub fn header_right(app: &mut AppState, ui: &mut Ui) {
    let scan = scan(app);
    if ui
        .button(format!("{} Open folder", icons::FOLDER))
        .on_hover_text(scan.models_dir.display().to_string())
        .clicked()
    {
        let _ = std::fs::create_dir_all(&scan.models_dir);
        if let Err(e) = util::open_path(&scan.models_dir) {
            app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", scan.models_dir.display()));
        }
    }
    let free = scan.free.map(|f| format!(" · {} free", runtime::fmt_bytes(f))).unwrap_or_default();
    ui.label(RichText::new(format!("{}{free}", short_path(&scan.models_dir, 3))).small().color(Palette::TEXT_DIM))
        .on_hover_text(format!(
            "Downloads and imports go to {}\n(Settings > Bonsai server > Models folder)",
            scan.models_dir.display()
        ));
}

/// The Models area body.
pub fn area(app: &mut AppState, ui: &mut Ui) {
    let scan = scan(app);
    let mut actions = Vec::new();
    let mut scroll = egui::ScrollArea::vertical().id_salt("models-scroll").auto_shrink([false, false]);
    if let Some(offset) = app.models.scroll_env {
        scroll = scroll.vertical_scroll_offset(offset);
    }
    scroll.show(ui, |ui| {
        egui::Frame::new().inner_margin(Margin::same(10)).show(ui, |ui| {
            ui.set_max_width(980.0);
            widgets::section_label(ui, "In use");
            card(ui, |ui| in_use(app, ui, &mut actions));
            if !app.models.tasks.is_empty() {
                widgets::section_label(ui, "Downloads and imports");
                card(ui, |ui| {
                    for (i, view) in app.models.tasks.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(4.0);
                            widgets::thin_separator(ui);
                            ui.add_space(4.0);
                        }
                        ui.push_id(("models-task", view.id), |ui| {
                            if let Some(a) = task_row(ui, view) {
                                actions.push(Action::Task(view.id, a));
                            }
                        });
                    }
                });
            }
            widgets::section_label(ui, "Server build (llama-server)");
            card(ui, |ui| server_section(app, ui, &scan, &mut actions));
            widgets::section_label(ui, "Models on this PC");
            card(ui, |ui| models_table(app, ui, &scan, &mut actions));
            widgets::section_label(ui, "Download a Bonsai 2 model");
            card(ui, |ui| catalog_table(app, ui, &scan, &mut actions));
            widgets::section_label(ui, "Any model on Hugging Face");
            card(ui, |ui| custom_form(app, ui, &mut actions));
        });
    });
    for action in actions {
        apply(app, action);
    }
}

fn apply(app: &mut AppState, action: Action) {
    match action {
        Action::Task(id, a) => task_action(app, id, a),
        Action::Start(task, then) => {
            start(app, task, then);
        }
        Action::UseModel(model, mmproj) => use_files(app, Some((model, mmproj)), None),
        Action::UseServer(exe) => use_files(app, None, Some(exe)),
        Action::BrowseModel => {
            let start_dir = app.config.runtime.model.clone();
            if let Some(path) = util::pick_file("Choose a model (.gguf)", "GGUF model", &["gguf"], start_dir.as_deref())
            {
                if runtime::is_model_file(&path) {
                    let mmproj = runtime::pair_mmproj(&path);
                    if mmproj.is_none() {
                        app.toast(ToastKind::Warn, "No *mmproj*.gguf next to the model: image input stays off");
                    }
                    use_files(app, Some((path, mmproj)), None);
                } else {
                    app.toast(
                        ToastKind::Error,
                        format!("{} is not a model file (a vision projector?)", file_name(&path)),
                    );
                }
            }
        }
        Action::BrowseServer => {
            let start_dir = app.config.runtime.server_exe.clone();
            if let Some(path) =
                util::pick_file("Choose llama-server.exe", "llama-server", &["exe"], start_dir.as_deref())
            {
                use_files(app, None, Some(path));
            }
        }
        Action::UseFolder => {
            if let Some(dir) = app.config.bonsai_dir.clone() {
                match app.set_bonsai_dir(dir) {
                    Ok(install) => {
                        app.toast(ToastKind::Ok, format!("Bonsai starts from {} again", install.root.display()))
                    }
                    Err(e) => app.toast(ToastKind::Error, format!("{e:#}")),
                }
            } else {
                app.show_setup = true;
            }
        }
        Action::Delete(model) => app.models.confirm_delete = Some(model),
        Action::Open(path) => {
            if let Err(e) = util::reveal_in_explorer(&path) {
                app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", path.display()));
            }
        }
        Action::Restart => {
            app.models.restart_hint = false;
            app.send(Command::RestartServer);
        }
        Action::ChooseBackend(backend) => app.models.chosen_backend = Some(backend),
    }
}

fn fact(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(LABEL_W, 20.0), Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(label).color(Palette::TEXT_DIM));
        });
        ui.add_space(4.0);
        add(ui);
    });
}

fn path_line(ui: &mut Ui, ok: bool, path: Option<&Path>, none: &str) {
    match path {
        Some(p) => {
            let (icon, color) = if ok { (icons::CHECK, Palette::OK) } else { (icons::CROSS, Palette::ERR) };
            ui.label(RichText::new(icon).color(color));
            ui.add(egui::Label::new(RichText::new(p.display().to_string()).monospace().size(12.0)).truncate())
                .on_hover_text(p.display().to_string());
        }
        None => {
            ui.label(RichText::new(icons::WARN).color(Palette::WARN));
            ui.label(RichText::new(none).color(Palette::TEXT_DIM));
        }
    }
}

fn in_use(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let install = app.status.install.clone();
    let running = matches!(app.status.state, ServerState::Ready | ServerState::Loading | ServerState::Starting);
    match install.as_ref().filter(|i| i.runtime) {
        Some(install) => {
            ui.label(
                RichText::new("The app's own runtime - no Bonsai-demo folder needed; files are used where they are")
                    .color(Palette::TEXT_DIM),
            );
            ui.add_space(4.0);
            fact(ui, "Server", |ui| {
                let exe = install.server_exe.as_deref();
                path_line(ui, exe.is_some_and(Path::is_file), exe, "no llama-server build yet - download one below");
            });
            fact(ui, "Model", |ui| {
                let model = install.models.first().map(PathBuf::as_path);
                path_line(ui, model.is_some_and(Path::is_file), model, "none chosen");
            });
            fact(ui, "Vision", |ui| {
                let mmproj = install.mmproj.as_deref();
                path_line(ui, mmproj.is_some_and(Path::is_file), mmproj, "no projector - image input is off");
            });
            if !install.missing_dlls.is_empty() {
                ui.add_space(4.0);
                vc_redist_note(ui, &install.missing_dlls);
            }
            // Missing files are marked on the lines above.
            let shown = ["Visual C++", "llama-server build", "llama-server.exe not found", "No model", "Model file"];
            for problem in install.problems().iter().filter(|p| !shown.iter().any(|s| p.contains(s))) {
                ui.label(RichText::new(format!("{} {problem}", icons::CROSS)).color(Palette::ERR));
            }
        }
        None => {
            let text = match (&install, &app.config.bonsai_dir) {
                (Some(i), _) => format!(
                    "Bonsai starts from the Bonsai-demo folder {}. Set a model active below to run without it.",
                    i.root.display()
                ),
                (None, _) => "Nothing chosen yet: download Bonsai below, or use a .gguf on this PC.".to_string(),
            };
            ui.add(egui::Label::new(RichText::new(text).color(Palette::TEXT_DIM)).wrap());
        }
    }
    if app.models.restart_hint && running {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} The new choice applies on the next start", icons::INFO)).color(Palette::WARN),
            );
            if ui
                .button(format!("{} Restart now", icons::RESTART))
                .on_hover_text("Stop Bonsai and start it with the new files")
                .clicked()
            {
                actions.push(Action::Restart);
            }
        });
    } else if !running {
        app.models.restart_hint = false;
    }
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        let browse = ui
            .button(format!("{} Use a .gguf on this PC...", icons::FOLDER))
            .on_hover_text("Pick a model file; it stays where it is (a *mmproj*.gguf next to it is used for images)");
        if browse.clicked() {
            actions.push(Action::BrowseModel);
        }
        if install.as_ref().is_some_and(|i| i.runtime) {
            let tip = match &app.config.bonsai_dir {
                Some(dir) => format!("Start Bonsai from {} again (start_llama_server.ps1)", dir.display()),
                None => "Choose a Bonsai-demo folder".to_string(),
            };
            if ui.button("Use the Bonsai-demo folder instead").on_hover_text(tip).clicked() {
                actions.push(Action::UseFolder);
            }
        }
    });
}

fn server_section(app: &mut AppState, ui: &mut Ui, scan: &Scan, actions: &mut Vec<Action>) {
    let choice = backend_choice(app);
    let wanted = app.models.chosen_backend.or(choice.as_ref().map(|c| c.backend));
    match &choice {
        Some(c) => {
            let color = if c.backend.is_gpu() { Palette::TEXT_DIM } else { Palette::WARN };
            ui.add(egui::Label::new(RichText::new(format!("For this PC: {}", c.reason)).color(color)).wrap());
        }
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Checking the GPU (nvidia-smi)...").color(Palette::TEXT_DIM));
            });
        }
    }
    ui.add_space(4.0);
    if let Some(backend) = wanted {
        let running = server_task(app, backend).is_some_and(TaskView::running);
        let installed = scan.builds.iter().any(|b| b.tag == runtime::RELEASE_TAG && b.backend == backend.id());
        ui.horizontal(|ui| {
            let mut pick = backend;
            egui::ComboBox::from_id_salt("models-backend")
                .selected_text(backend.label())
                .width(120.0)
                .show_ui(ui, |ui| {
                    for b in Backend::ALL {
                        let recommended = choice.as_ref().is_some_and(|c| c.backend == b);
                        let label =
                            if recommended { format!("{} (for this PC)", b.label()) } else { b.label().to_string() };
                        ui.selectable_value(&mut pick, b, label);
                    }
                })
                .response
                .on_hover_text(
                    "CUDA needs an NVIDIA driver (13.3 for CUDA 13.3 or newer); Vulkan works on most GPUs; CPU is slow",
                );
            if pick != backend {
                actions.push(Action::ChooseBackend(pick));
            }
            let label = format!("Download ({})", runtime::fmt_bytes(backend.download_bytes()));
            let tip = format!(
                "PrismML's llama.cpp {} ({}): llama-server.exe and its DLLs{}",
                runtime::RELEASE_TAG,
                backend.label(),
                if matches!(backend, Backend::Cuda124 | Backend::Cuda133) { " + the CUDA runtime" } else { "" }
            );
            let button = ui.add_enabled(!running, egui::Button::new(format!("⏬ {label}")));
            if button.on_hover_text(tip).on_disabled_hover_text("Downloading...").clicked() {
                actions.push(Action::Start(RuntimeTask::DownloadServer(backend), Then::UseServer));
            }
            if installed {
                widgets::badge(ui, "downloaded", Palette::OK);
            }
        });
    }
    ui.add_space(4.0);
    let active_exe = app.status.install.as_ref().filter(|i| i.runtime).and_then(|i| i.server_exe.clone());
    if scan.builds.is_empty() {
        ui.label(RichText::new("No server build in the app yet.").color(Palette::TEXT_DIM));
    }
    for build in &scan.builds {
        ui.horizontal(|ui| {
            ui.label(RichText::new(icons::SERVER).color(Palette::TEXT_DIM));
            ui.label(format!("{} · {}", build.backend, build.tag));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if active_exe.as_deref().is_some_and(|e| runtime::same_file(e, &build.exe)) {
                    widgets::badge(ui, "in use", Palette::OK);
                } else if ui.button("Use").on_hover_text("Start Bonsai with this build").clicked() {
                    actions.push(Action::UseServer(build.exe.clone()));
                }
                if widgets::icon_button(ui, icons::FOLDER, &build.dir.display().to_string()).clicked() {
                    actions.push(Action::Open(build.exe.clone()));
                }
            });
        });
    }
    if ui
        .button(format!("{} Use a llama-server.exe on this PC...", icons::FOLDER))
        .on_hover_text(
            "E.g. bin\\cuda\\llama-server.exe of a Bonsai-demo folder - it must be PrismML's build (prism-b...)",
        )
        .clicked()
    {
        actions.push(Action::BrowseServer);
    }
}

fn models_table(app: &mut AppState, ui: &mut Ui, scan: &Scan, actions: &mut Vec<Action>) {
    if scan.models.is_empty() {
        ui.label(
            RichText::new("No models yet - download one below, or use a .gguf on this PC.").color(Palette::TEXT_DIM),
        );
        return;
    }
    let active = app.config.runtime.model.clone().filter(|_| app.config.runtime.is_active());
    let busy: Vec<PathBuf> =
        app.models.tasks.iter().filter(|t| t.running()).flat_map(|t| t.task.targets(&app.config)).collect();
    egui::Grid::new("models-installed").num_columns(4).spacing([14.0, 4.0]).min_row_height(22.0).show(ui, |ui| {
        for m in &scan.models {
            let is_active = active.as_deref().is_some_and(|a| runtime::same_file(a, &m.path));
            ui.horizontal(|ui| {
                ui.label(RichText::new(&m.id).color(Palette::TEXT)).on_hover_text(m.path.display().to_string());
                if is_active {
                    widgets::badge(ui, "active", Palette::OK);
                }
            });
            ui.label(RichText::new(runtime::fmt_bytes(m.bytes)).color(Palette::TEXT_DIM));
            let vision = match &m.mmproj {
                Some(_) => RichText::new(format!("{} vision", icons::CHECK)).color(Palette::OK),
                None => RichText::new("no vision").color(Palette::TEXT_FAINT),
            };
            let place = if m.managed { "in the app" } else { "in place" };
            ui.label(vision).on_hover_text(format!(
                "{}\n{place}: {}",
                m.mmproj.as_ref().map_or("No *mmproj*.gguf next to it".to_string(), |p| p.display().to_string()),
                m.path.parent().map(|p| p.display().to_string()).unwrap_or_default()
            ));
            ui.horizontal(|ui| {
                let replacing = busy.iter().any(|b| runtime::same_file(b, &m.path));
                if !is_active {
                    let r = ui.add_enabled(!replacing, egui::Button::new("Set active"));
                    if r.on_hover_text("Bonsai uses this model from the next start").clicked() {
                        actions.push(Action::UseModel(m.path.clone(), m.mmproj.clone()));
                    }
                }
                if widgets::icon_button(ui, icons::FOLDER, "Show in Explorer").clicked() {
                    actions.push(Action::Open(m.path.clone()));
                }
                if m.managed {
                    let del = ui.add_enabled(
                        !is_active && !replacing,
                        egui::Button::new(icons::TRASH).min_size(egui::vec2(24.0, 22.0)),
                    );
                    if del
                        .on_hover_text("Delete the file (asks first)")
                        .on_disabled_hover_text("The active model cannot be deleted - set another one active first")
                        .clicked()
                    {
                        actions.push(Action::Delete(m.clone()));
                    }
                }
            });
            ui.end_row();
        }
    });
}

fn catalog_table(app: &mut AppState, ui: &mut Ui, scan: &Scan, actions: &mut Vec<Action>) {
    /// Room for the size and the buttons on the right of each row.
    const RIGHT_W: f32 = 370.0;
    for (i, m) in runtime::CATALOG.iter().enumerate() {
        if i > 0 {
            ui.add_space(3.0);
            widgets::thin_separator(ui);
            ui.add_space(3.0);
        }
        let installed = scan.models.iter().find(|x| x.catalog_id == Some(m.id) && x.managed);
        let task = RuntimeTask::DownloadModel(m.source());
        let running = app.models.tasks.iter().any(|t| t.running() && t.task == task);
        ui.horizontal(|ui| {
            let left_w = (ui.available_width() - RIGHT_W).max(160.0);
            ui.allocate_ui_with_layout(egui::vec2(left_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(left_w);
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new(m.title).color(Palette::TEXT))
                    .on_hover_text(format!("{} / {}", m.repo, m.model.name));
                ui.add(egui::Label::new(RichText::new(m.note).small().color(Palette::TEXT_DIM)).wrap());
            });
            ui.add_sized(
                [56.0, 20.0],
                egui::Label::new(RichText::new(runtime::fmt_bytes(m.total_bytes())).color(Palette::TEXT_DIM)),
            )
            .on_hover_text("Model + vision projector");
            match installed {
                Some(found) => {
                    widgets::badge(ui, "downloaded", Palette::OK);
                    let active =
                        app.config.runtime.model.as_deref().is_some_and(|a| runtime::same_file(a, &found.path));
                    if !active && ui.button("Set active").clicked() {
                        actions.push(Action::UseModel(found.path.clone(), found.mmproj.clone()));
                    }
                }
                None if running => {
                    ui.label(RichText::new("downloading...").color(Palette::INFO));
                }
                None => {
                    if ui.button("⏬ Download").on_hover_text("Download only").clicked() {
                        actions.push(Action::Start(task.clone(), Then::Keep));
                    }
                    if ui.button("Download and use").on_hover_text("Bonsai uses it once it is here").clicked() {
                        actions.push(Action::Start(task, Then::UseModel));
                    }
                }
            }
        });
    }
}

fn custom_form(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let st = &mut app.models;
    ui.label(
        RichText::new("Any GGUF that PrismML's llama.cpp loads, e.g. a fine-tuned Bonsai 2. A gated repo needs a token (Settings > Bonsai server).")
            .small()
            .color(Palette::TEXT_DIM),
    );
    ui.add_space(4.0);
    let field = |ui: &mut Ui, label: &str, value: &mut String, hint: &str| {
        widgets::prop_row(ui, label, |ui| {
            ui.add(
                egui::TextEdit::singleline(value).hint_text(hint).font(theme::mono(12.0)).desired_width(f32::INFINITY),
            );
        });
    };
    field(ui, "Repo", &mut st.hf_repo, "owner/name, e.g. prism-ml/Ternary-Bonsai-2-27B-gguf");
    field(ui, "Model file", &mut st.hf_file, "e.g. Ternary-Bonsai-2-27B-PTQ1_0.gguf");
    field(ui, "Vision projector", &mut st.hf_mmproj, "optional, e.g. Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf");
    let spec = format!("hf:{}/{}", st.hf_repo.trim().trim_matches('/'), st.hf_file.trim());
    let mmproj = st.hf_mmproj.trim();
    let parsed = (!st.hf_repo.trim().is_empty() && !st.hf_file.trim().is_empty())
        .then(|| ModelSource::parse(&spec, Some(if mmproj.is_empty() { "none" } else { mmproj })));
    widgets::prop_row(ui, "", |ui| {
        let ok = matches!(parsed, Some(Ok(_)));
        let tip = match &parsed {
            Some(Err(e)) => format!("{e:#}"),
            None => "Fill in the repo and the file".to_string(),
            Some(Ok(_)) => {
                "Download into the models folder (sizes and checksums from the repo's file list)".to_string()
            }
        };
        let r = ui.add_enabled(ok, egui::Button::new("⏬ Download")).on_hover_text(&tip).on_disabled_hover_text(&tip);
        if r.clicked() {
            if let Some(Ok(source)) = &parsed {
                actions.push(Action::Start(RuntimeTask::DownloadModel(source.clone()), Then::Keep));
            }
        }
        if let Some(Err(e)) = &parsed {
            ui.label(RichText::new(format!("{e:#}")).small().color(Palette::ERR));
        }
    });
}

/// "Delete this model?" (drawn every frame by the app shell while it is asked).
pub fn delete_dialog(app: &mut AppState, ctx: &egui::Context) {
    let Some(model) = app.models.confirm_delete.clone() else { return };
    let modal = egui::Modal::new(egui::Id::new("bh3-delete-model")).frame(widgets::dialog_frame()).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.label(RichText::new(format!("{} Delete {}?", icons::TRASH, model.id)).size(15.5).strong());
        ui.add_space(6.0);
        let mut size = model.bytes;
        let mut lines = vec![model.path.display().to_string()];
        if let Some(p) = &model.mmproj {
            lines.push(format!("{} (deleted too unless another model of the folder uses it)", file_name(p)));
            size += model.mmproj_bytes.unwrap_or(0);
        }
        for line in lines {
            ui.add(egui::Label::new(RichText::new(line).small().color(Palette::TEXT_DIM)).wrap());
        }
        ui.label(format!("Frees up to {}. Downloading it again takes a while.", runtime::fmt_bytes(size)));
        ui.add_space(12.0);
        let mut choice = None;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let delete = egui::Button::new(RichText::new("Delete").strong().color(Color32::WHITE))
                .fill(Palette::ERR.lerp_to_gamma(Palette::WIDGET, 0.3))
                .min_size(egui::vec2(96.0, 26.0));
            if ui.add(delete).clicked() {
                choice = Some(true);
            }
            if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(90.0, 26.0))).clicked() {
                choice = Some(false);
            }
        });
        choice
    });
    let choice = match modal.inner {
        Some(c) => Some(c),
        None if modal.should_close() => Some(false),
        None => None,
    };
    match choice {
        Some(true) => {
            app.models.confirm_delete = None;
            match runtime::remove_model(&model, &runtime::models_dir(&app.config)) {
                Ok(removed) => app.toast(ToastKind::Ok, format!("Deleted {} file(s) of {}", removed.len(), model.id)),
                Err(e) => app.toast(ToastKind::Error, format!("{e:#}")),
            }
            app.models.dirty = true;
        }
        Some(false) => app.models.confirm_delete = None,
        None => {}
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The last `parts` components of a path (`…\bin\b1\llama-server.exe`).
pub fn short_path(path: &Path, parts: usize) -> String {
    let comps: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if comps.len() <= parts + 1 {
        return path.display().to_string();
    }
    format!("…\\{}", comps[comps.len() - parts..].join("\\"))
}
