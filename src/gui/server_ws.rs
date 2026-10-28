//! Bonsai workspace: install info, start/stop, launcher options, live log, GPU.
//!
//! Left: Install (the Bonsai-demo folder and what was found in it, or the app's own runtime) and
//! Launch options (saved at once; context, projector and env changes apply on the next start).
//! Centre: a status card with Start / Stop / Restart / Open web UI / Free GPU for ComfyUI, a GPU
//! card (VRAM, load, temperature, processes) and below either the live server log (filter,
//! autoscroll, wrap, copy, open file) or the Models area ([`super::models_ws`]), switched with the
//! tabs in that area's header.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bh3::config::EnvVar;
use bh3::gpu::{self, GpuInfo};
use bh3::server::{BonsaiInstall, ServerState};
use bh3::service::{Command, Event};
use eframe::egui::{
    self, text::LayoutJob, Align, Color32, CornerRadius, FontId, Layout, Margin, RichText, Sense, Stroke, TextFormat,
    Ui,
};

use super::app::{AppState, ToastKind};
use super::models_ws;
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Width of the dim labels in the status and GPU cards.
const FACT_LABEL_W: f32 = 74.0;
/// Below this centre width the status and GPU cards stack instead of sitting side by side.
const TWO_CARDS_MIN_W: f32 = 760.0;
/// Context sizes offered for `BONSAI_CTX` (0 = the launcher's RAM-based default).
const CTX_CHOICES: [u32; 5] = [0, 8192, 16384, 32768, 65536];
/// KV cache of the 27B per context token (FP16): 64 KiB, so 16k tokens ~ 1 GiB.
const KV_KIB_PER_TOKEN: u64 = 64;
/// VRAM llama-server needs besides weights and KV cache (compute buffers), measured on the 4060.
const VRAM_OVERHEAD_MB: u64 = 450;
/// What the launcher picks with `BONSAI_CTX` unset on a 12-23 GB machine.
const LAUNCHER_DEFAULT_CTX: u32 = 16384;
/// Start-time estimates of one run differ by less than this (an estimate is up to ~2.5 s late:
/// the status is up to a second old and the uptime counts whole seconds).
const SAME_RUN_SECS: f64 = 4.0;
/// How often the log file's existence is checked.
const FILE_CHECK_SECS: f64 = 2.0;
/// Monospace size of log lines.
const LOG_FONT: f32 = 12.0;
/// Width of the "=" between an env var's name and value.
const ENV_EQ_W: f32 = 8.0;
/// Process names longer than this are shortened in the GPU table (full name on hover).
const PROCESS_NAME_CHARS: usize = 26;

/// What the bottom area of the centre shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BottomView {
    #[default]
    Log,
    Models,
}

/// What the user asked for this frame (applied after drawing, when nothing is borrowed).
enum Action {
    ShowModels,
    Start,
    Stop,
    Restart,
    OpenWebUi,
    ChangeFolder,
    OpenPath(PathBuf),
    Rescan(PathBuf),
    OpenLog,
    CopyLog(Vec<String>),
    /// Save the config (context, projector and env changes apply on the next start).
    SaveConfig,
}

/// The running server as this workspace knows it, for "Changed - restart Bonsai to apply".
struct SeenRun {
    /// When it started (`app.time`), estimated from the status uptime and never earlier than the
    /// real start; `None` for a server started outside the app (it reports no uptime).
    started: Option<f64>,
    /// `config.launcher_env()` when this run was first seen: what its launcher got, as far as this
    /// app knows.
    env: Vec<(String, String)>,
}

pub struct ServerWsState {
    /// Log filter (case-insensitive).
    log_filter: String,
    /// Keep the log scrolled to the newest line.
    autoscroll: bool,
    /// Wrap long log lines.
    wrap: bool,
    /// Scroll to the end once (autoscroll was switched on).
    jump_to_end: bool,
    /// The running server and its launch settings (`None` while it is not running).
    run: Option<SeenRun>,
    /// Natural heights of the status and GPU cards last frame (both are drawn as tall as the taller).
    card_heights: [f32; 2],
    /// Natural height of the whole status + GPU area last frame (sizes its panel).
    cards_area_h: f32,
    /// `config_dir()\server.log` (looked up once: `config_dir()` touches the disk).
    log_path: Option<PathBuf>,
    /// Existence checks, at most every [`FILE_CHECK_SECS`]: path -> (checked at, exists).
    file_checks: HashMap<PathBuf, (f64, bool)>,
    /// Sizes of model files in bytes, read once per path.
    sizes: HashMap<PathBuf, Option<u64>>,
    /// Why the configured folder is not usable: (folder, error of `BonsaiInstall::detect`).
    folder_error: Option<(PathBuf, String)>,
    /// Log or Models below the cards.
    view: BottomView,
}

impl ServerWsState {
    /// Show the Models area below the cards.
    pub fn show_models(&mut self) {
        self.view = BottomView::Models;
    }
}

impl Default for ServerWsState {
    fn default() -> Self {
        Self {
            log_filter: String::new(),
            autoscroll: true,
            wrap: true,
            jump_to_end: false,
            run: None,
            card_heights: [0.0; 2],
            cards_area_h: 0.0,
            log_path: None,
            file_checks: HashMap::new(),
            sizes: HashMap::new(),
            folder_error: None,
            view: BottomView::Log,
        }
    }
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    track_run(app);
    let mut actions = Vec::new();

    egui::SidePanel::left("srv-left")
        .resizable(true)
        .default_width(320.0)
        .width_range(280.0..=460.0)
        .frame(egui::Frame::new().fill(Palette::SIDE).inner_margin(Margin::same(8)))
        .show(ctx, |ui| {
            egui::ScrollArea::vertical().id_salt("srv-left-scroll").auto_shrink([false, false]).show(ui, |ui| {
                install_panel(app, ui, &mut actions);
                launch_panel(app, ui, &mut actions);
            });
        });

    egui::CentralPanel::default().frame(egui::Frame::new().fill(Palette::GAP)).show(ctx, |ui| {
        // As tall as the cards (measured last frame), but never more than ~60 % of the height:
        // the log keeps its room and the cards scroll instead.
        let max_cards_h = (ui.available_height() * 0.6).max(140.0);
        let wanted = app.server_ws.cards_area_h;
        let cards_h = if wanted > 0.0 { wanted.min(max_cards_h) } else { max_cards_h.min(400.0) };
        egui::TopBottomPanel::top("srv-cards")
            .exact_height(cards_h)
            .frame(egui::Frame::new().fill(Palette::REGION).outer_margin(Margin { bottom: 2, ..Margin::ZERO }))
            .show_separator_line(false)
            .show_inside(ui, |ui| cards_area(app, ui, &mut actions));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(Palette::REGION)).show_inside(ui, |ui| {
            match app.server_ws.view {
                BottomView::Log => log_area(app, ui, &mut actions),
                BottomView::Models => {
                    let mut view = app.server_ws.view;
                    bottom_header(ui, &mut view, |ui| models_ws::header_right(app, ui));
                    app.server_ws.view = view;
                    models_ws::area(app, ui);
                }
            }
        });
    });

    if !actions.is_empty() {
        // Applied after drawing: show the result right away.
        ctx.request_repaint();
    }
    for action in actions {
        apply(app, ctx, action);
    }
}

/// Status, GPU and log arrive in `Event::Status` (handled by the app shell); nothing else here.
pub fn on_event(app: &mut AppState, ev: &Event) {
    let _ = (app, ev);
}

/// Remember the launch settings of the running server, so "Changed - restart Bonsai to apply"
/// ends with any restart, wherever it was triggered (power switch, idle auto-stop, auto-start).
/// Cheap: `show` calls it every frame; calling it from the app shell every frame as well would
/// also catch changes made in Settings before this workspace first saw the run.
pub fn track_run(app: &mut AppState) {
    let s = &app.status;
    if !is_running(&s.state) {
        // Stopped (or restarting): the next start uses the saved options.
        app.server_ws.run = None;
        return;
    }
    let started = s.uptime_secs.map(|up| app.time - up as f64);
    if let Some(run) = app.server_ws.run.as_mut().filter(|run| same_run(run.started, started)) {
        // The earliest estimate is the closest to the real start.
        run.started = run.started.zip(started).map(|(seen, now)| seen.min(now));
        return;
    }
    app.server_ws.run = Some(SeenRun { started, env: app.config.launcher_env() });
}

/// Start estimates of one run (both `None`: the same server started outside the app).
fn same_run(seen: Option<f64>, now: Option<f64>) -> bool {
    match (seen, now) {
        (Some(seen), Some(now)) => (now - seen).abs() < SAME_RUN_SECS,
        (None, None) => true,
        _ => false,
    }
}

fn apply(app: &mut AppState, ctx: &egui::Context, action: Action) {
    match action {
        Action::ShowModels => app.server_ws.view = BottomView::Models,
        Action::Start => app.send(Command::StartServer),
        Action::Stop => app.send(Command::StopServer),
        Action::Restart => {
            app.send(Command::RestartServer);
            // Seen again with the saved options until the restart shows (no warning meanwhile).
            app.server_ws.run = None;
        }
        Action::OpenWebUi => util::open_url(ctx, &app.config.base_url()),
        Action::ChangeFolder => app.show_setup = true,
        Action::OpenPath(path) => {
            if let Err(e) = util::open_path(&path) {
                app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", path.display()));
            }
        }
        Action::Rescan(dir) => {
            // Forget what was read from the folder (setup.ps1 may have replaced models).
            app.server_ws.folder_error = None;
            app.server_ws.sizes.clear();
            app.server_ws.file_checks.clear();
            match app.set_bonsai_dir(dir) {
                Ok(install) if install.is_usable() => app.toast(ToastKind::Ok, "Bonsai folder checked: ready to start"),
                Ok(install) => {
                    app.toast(ToastKind::Warn, format!("Bonsai folder checked: {}", install.problems().join("; ")))
                }
                Err(e) => app.toast(ToastKind::Error, format!("{e:#}")),
            }
        }
        Action::OpenLog => {
            let path = log_path(app);
            if path.is_file() {
                if let Err(e) = util::open_path(&path) {
                    app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", path.display()));
                }
            } else {
                app.toast(ToastKind::Warn, "No log file yet - it is written when this app starts Bonsai");
            }
        }
        Action::CopyLog(lines) => {
            let n = lines.len();
            ctx.copy_text(lines.join("\n"));
            app.toast(ToastKind::Info, format!("Copied {n} log line{}", if n == 1 { "" } else { "s" }));
        }
        Action::SaveConfig => app.save_config(),
    }
}

// ---------------------------------------------------------------------------------------------
// Left: install

fn install_panel(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let install = app.status.install.clone();
    let folder = install.as_ref().map(|i| i.root.clone()).or_else(|| app.config.bonsai_dir.clone());
    let folder_error = match (&install, app.config.bonsai_dir.clone()) {
        (None, Some(dir)) => Some(folder_error(app, &dir)),
        _ => None,
    };
    let sizes = install.as_ref().map(|i| model_sizes(app, i)).unwrap_or_default();
    if let Some(install) = install.as_ref().filter(|i| i.runtime) {
        runtime_panel(ui, install, &sizes, actions);
        return;
    }

    widgets::panel(ui, "srv-install", "Install", true, |ui| {
        widgets::section_label(ui, "Bonsai folder");
        match &folder {
            Some(dir) => {
                let text = dir.display().to_string();
                ui.add(egui::Label::new(RichText::new(&text).monospace().color(Palette::TEXT)).wrap())
                    .on_hover_text("The Bonsai-demo folder this app starts llama-server from");
            }
            None => {
                ui.label(RichText::new("No folder chosen yet").color(Palette::WARN));
            }
        }
        ui.horizontal(|ui| {
            let change = ui
                .button("Change...")
                .on_hover_text("Choose the Bonsai-demo folder, model files on this PC, or download Bonsai");
            if change.clicked() {
                actions.push(Action::ChangeFolder);
            }
            let open = ui
                .add_enabled(folder.is_some(), egui::Button::new(format!("{} Open folder", icons::FOLDER)))
                .on_hover_text("Open the folder in Explorer")
                .on_disabled_hover_text("Choose a folder first");
            if open.clicked() {
                if let Some(dir) = &folder {
                    actions.push(Action::OpenPath(dir.clone()));
                }
            }
            let rescan = ui
                .add_enabled(folder.is_some(), egui::Button::new(icons::RESTART).min_size(egui::vec2(24.0, 22.0)))
                .on_hover_text("Check the folder again (e.g. after running setup.ps1)")
                .on_disabled_hover_text("Choose a folder first");
            if rescan.clicked() {
                if let Some(dir) = &folder {
                    actions.push(Action::Rescan(dir.clone()));
                }
            }
        });

        let Some(install) = &install else {
            ui.add_space(4.0);
            let text = folder_error.unwrap_or_else(|| {
                "Choose the folder that holds start_bonsai.bat, scripts\\, bin\\ and models\\.".into()
            });
            ui.add(egui::Label::new(RichText::new(text).color(Palette::ERR)).wrap());
            return;
        };

        ui.add_space(6.0);
        widgets::section_label(ui, "Detected");
        parts(ui, install, &sizes);

        let problems = install.problems();
        let warnings = install.warnings();
        ui.add_space(4.0);
        if problems.is_empty() && warnings.is_empty() {
            ui.label(RichText::new(format!("{} Ready to start from this folder", icons::CHECK)).color(Palette::OK));
        }
        for problem in &problems {
            note_line(ui, icons::CROSS, problem, Palette::ERR);
        }
        for warning in &warnings {
            note_line(ui, icons::WARN, warning, Palette::WARN);
        }
    });
}

/// Install panel of the app's own runtime: server, model and projector where they are.
fn runtime_panel(
    ui: &mut Ui,
    install: &BonsaiInstall,
    sizes: &HashMap<PathBuf, Option<u64>>,
    actions: &mut Vec<Action>,
) {
    widgets::panel(ui, "srv-install", "Install", true, |ui| {
        widgets::section_label(ui, "App runtime");
        ui.add(
            egui::Label::new(
                RichText::new("No Bonsai-demo folder needed: these files are started directly, where they are.")
                    .small()
                    .color(Palette::TEXT_DIM),
            )
            .wrap(),
        );
        ui.add_space(4.0);
        let sized = |p: &Path| match sizes.get(p).copied().flatten() {
            Some(bytes) => format!("{} · {}", file_name(p), fmt_bytes(bytes)),
            None => file_name(p),
        };
        let folder = |p: &Path| models_ws::short_path(p.parent().unwrap_or(p), 2);
        match &install.server_exe {
            Some(p) if p.is_file() => part_row(
                ui,
                Part::Found,
                "llama-server.exe",
                &[format!("{} · {}", folder(p), install.backend().unwrap_or("?"))],
            ),
            Some(p) => part_row(ui, Part::Missing, "llama-server.exe", &[format!("{} not found", p.display())]),
            None => part_row(ui, Part::Missing, "llama-server.exe", &["not installed - download it in Models".into()]),
        }
        match install.models.first() {
            Some(p) if p.is_file() => part_row(ui, Part::Found, "Model", &[sized(p), format!("in {}", folder(p))]),
            Some(p) => part_row(ui, Part::Missing, "Model", &[format!("{} not found", p.display())]),
            None => part_row(ui, Part::Missing, "Model", &["none chosen".into()]),
        }
        match &install.mmproj {
            Some(p) if p.is_file() => part_row(ui, Part::Found, "Vision projector", &[sized(p)]),
            _ => part_row(ui, Part::Warn, "Vision projector", &["none - image input is off".into()]),
        }
        ui.add_space(4.0);
        let problems = install.problems();
        if problems.is_empty() {
            ui.label(RichText::new(format!("{} Ready to start", icons::CHECK)).color(Palette::OK));
        }
        if !install.missing_dlls.is_empty() {
            models_ws::vc_redist_note(ui, &install.missing_dlls);
        }
        // Missing files are marked on the lines above.
        let shown = ["Visual C++", "llama-server build", "llama-server.exe not found", "No model", "Model file"];
        for problem in problems.iter().filter(|p| !shown.iter().any(|s| p.contains(s))) {
            note_line(ui, icons::CROSS, problem, Palette::ERR);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Models...").on_hover_text("Download, choose or delete models and server builds").clicked() {
                actions.push(Action::ShowModels);
            }
            if ui
                .button("Change...")
                .on_hover_text("Setup: model files on this PC, download, or a Bonsai-demo folder")
                .clicked()
            {
                actions.push(Action::ChangeFolder);
            }
        });
    });
}

#[derive(Clone, Copy)]
enum Part {
    Found,
    Missing,
    Warn,
    Optional,
}

/// The pieces of the install with check marks.
fn parts(ui: &mut Ui, install: &BonsaiInstall, sizes: &HashMap<PathBuf, Option<u64>>) {
    let rel = |p: &Path| p.strip_prefix(&install.root).unwrap_or(p).display().to_string();
    let sized = |p: &Path| match sizes.get(p).copied().flatten() {
        Some(bytes) => format!("{} · {}", file_name(p), fmt_bytes(bytes)),
        None => file_name(p),
    };

    match &install.launcher_ps1 {
        Some(p) => part_row(ui, Part::Found, "Launcher script", &[rel(p)]),
        None if install.server_exe.is_some() => part_row(
            ui,
            Part::Warn,
            "Launcher script",
            &[r"scripts\start_llama_server.ps1 not found - started directly".into()],
        ),
        None => part_row(ui, Part::Missing, "Launcher script", &[r"scripts\start_llama_server.ps1 not found".into()]),
    }
    match &install.launcher_bat {
        Some(p) => part_row(ui, Part::Found, "start_bonsai.bat", &[format!("{} (your own launcher)", rel(p))]),
        None => part_row(ui, Part::Optional, "start_bonsai.bat", &["not found (optional)".into()]),
    }
    match &install.server_exe {
        Some(p) => part_row(
            ui,
            Part::Found,
            "llama-server.exe",
            &[format!("{} · {}", rel(p), install.backend().unwrap_or("?"))],
        ),
        None => part_row(
            ui,
            Part::Missing,
            "llama-server.exe",
            &[r"not found in bin\cuda, bin\hip, bin\vulkan or bin\cpu".into()],
        ),
    }
    if install.models.is_empty() {
        part_row(ui, Part::Missing, "Model", &[r"no *.gguf under models\".into()]);
    } else {
        let primary = install.primary_model();
        let mut lines = Vec::new();
        if let Some(p) = primary {
            lines.push(format!("{} (launcher pick)", sized(p)));
        }
        let others: Vec<&PathBuf> = install.models.iter().filter(|m| Some(*m) != primary).collect();
        for p in others.iter().take(3) {
            lines.push(sized(p));
        }
        if others.len() > 3 {
            lines.push(format!("+ {} more under models\\", others.len() - 3));
        }
        let title =
            if install.models.len() == 1 { "Model".to_string() } else { format!("Models ({})", install.models.len()) };
        part_row(ui, Part::Found, &title, &lines);
    }
    match &install.mmproj {
        Some(p) => part_row(ui, Part::Found, "Vision projector", &[sized(p)]),
        None => part_row(ui, Part::Warn, "Vision projector", &["none - image input is disabled".into()]),
    }
}

/// Check mark (or cross) + title, with dim detail lines below.
fn part_row(ui: &mut Ui, part: Part, title: &str, details: &[String]) {
    let (icon, color) = match part {
        Part::Found => (icons::CHECK, Palette::OK),
        Part::Missing => (icons::CROSS, Palette::ERR),
        Part::Warn => (icons::WARN, Palette::WARN),
        Part::Optional => ("–", Palette::TEXT_FAINT),
    };
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(16.0, 18.0), Layout::left_to_right(Align::Center), |ui| {
            ui.label(RichText::new(icon).color(color));
        });
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(title).color(Palette::TEXT));
            for detail in details {
                ui.add(egui::Label::new(RichText::new(detail).small().color(Palette::TEXT_DIM)).wrap());
            }
        });
    });
}

fn note_line(ui: &mut Ui, icon: &str, text: &str, color: Color32) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(icon).color(color));
        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
    });
}

// ---------------------------------------------------------------------------------------------
// Left: launch options

fn launch_panel(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let running = is_running(&app.status.state);
    let can_restart = matches!(app.status.state, ServerState::Ready | ServerState::Loading);
    let running_ctx = app.status.props.as_ref().map(|p| p.n_ctx).filter(|&n| n > 0);
    let weights_mb = app.status.install.clone().map(|i| weights_mb(app, &i));
    let gpu_total_mb = app.status.gpu.as_ref().map(|g| g.mem_total_mb).filter(|&t| t > 0);
    let run_env = app.server_ws.run.as_ref().map(|run| run.env.clone());

    widgets::panel(ui, "srv-launch", "Launch options", true, |ui| {
        let cfg = &mut app.config;
        let mut save = false;

        widgets::prop_row(ui, "Auto-start", |ui| {
            save |= ui
                .checkbox(&mut cfg.auto_start, "When a prompt needs it")
                .on_hover_text("Start Bonsai by itself when you write a prompt or send a chat message")
                .changed();
        });
        widgets::prop_row(ui, "Auto-stop", |ui| {
            let r = ui
                .add(egui::DragValue::new(&mut cfg.auto_stop_idle_minutes).range(0..=240).speed(0.2).suffix(" min"))
                .on_hover_text(
                    "Stop Bonsai after this many idle minutes if this app started it - frees the GPU for ComfyUI",
                );
            save |= committed(&r);
            ui.label(RichText::new("0 = never").small().color(Palette::TEXT_DIM));
        });
        widgets::prop_row(ui, "Stop on exit", |ui| {
            save |= ui
                .checkbox(&mut cfg.stop_on_exit, "If this app started it")
                .on_hover_text("Stop Bonsai when this app closes (only a server this app started)")
                .changed();
        });
        widgets::prop_row(ui, "VRAM busy", |ui| {
            let r = ui
                .add(egui::DragValue::new(&mut cfg.vram_busy_mb).range(0..=16384).speed(10.0).suffix(" MB"))
                .on_hover_text(
                    "Auto-start waits while other programs (e.g. ComfyUI) use more VRAM than this.\n\
                     0 = never wait. The Start button always starts.",
                );
            save |= committed(&r);
            ui.label(RichText::new("0 = off").small().color(Palette::TEXT_DIM));
        });
        widgets::prop_row(ui, "Context", |ui| {
            let before = cfg.ctx_size;
            egui::ComboBox::from_id_salt("srv-ctx")
                .width(ui.available_width())
                .selected_text(ctx_label(cfg.ctx_size))
                .show_ui(ui, |ui| {
                    let mut choices = CTX_CHOICES.to_vec();
                    if !choices.contains(&cfg.ctx_size) {
                        choices.push(cfg.ctx_size);
                    }
                    for choice in choices {
                        ui.selectable_value(&mut cfg.ctx_size, choice, ctx_label(choice));
                    }
                })
                .response
                .on_hover_text("BONSAI_CTX: how many tokens Bonsai holds (system prompt + request + answer)");
            save |= cfg.ctx_size != before;
        });
        // What the launcher gets decides the estimate (extra env wins over the options above).
        let need = vram_need(weights_mb.unwrap_or_default(), &cfg.launcher_env(), running_ctx);
        widgets::prop_row(ui, "", |ui| {
            let note = if need.auto_ctx {
                "Auto: 16k with 12-23 GB RAM".to_string()
            } else {
                let kv = if need.kv4 { " (q4_0)" } else { "" };
                format!("≈ {:.1} GiB of VRAM for the context{kv}", need.context as f64 / 1024.0)
            };
            ui.add(egui::Label::new(RichText::new(note).small().color(Palette::TEXT_DIM)).wrap()).on_hover_text(
                "The KV cache takes ~64 KiB of VRAM per token: 16k ≈ 1 GiB, 32k ≈ 2 GiB.\n\
                 Auto lets the launcher choose by RAM (≤11 GB: 8k, 12-23 GB: 16k, 24-35 GB: 32k).",
            );
        });
        if let (Some(_), Some(total)) = (weights_mb, gpu_total_mb) {
            let total = u64::from(total);
            // With BONSAI_NGL some layers may stay in RAM: the estimate is only an upper bound.
            let too_big = need.total > total && !need.ngl_set;
            let color = if too_big {
                Palette::ERR
            } else if need.total + 200 > total && !need.ngl_set {
                Palette::WARN
            } else {
                Palette::TEXT_DIM
            };
            widgets::prop_row(ui, "", |ui| {
                let at_most = if need.ngl_set { "at most " } else { "" };
                let text = format!("Needs {at_most}≈ {} of the {} GPU", fmt_mb(need.total), fmt_mb(total));
                let mut hover = format!(
                    "Model {} + vision projector {} + context {}{} + buffers ~{}",
                    fmt_mb(need.model),
                    fmt_mb(need.projector),
                    fmt_mb(need.context),
                    if need.kv4 { " (q4_0 KV cache, BONSAI_KV4)" } else { "" },
                    fmt_mb(VRAM_OVERHEAD_MB),
                );
                if need.auto_ctx && running_ctx.is_none() {
                    hover.push_str("\nAuto context counted as 16k (the launcher picks it by RAM).");
                }
                if need.ngl_set {
                    hover.push_str("\nBONSAI_NGL is set: layers kept in system RAM need less VRAM.");
                }
                if too_big {
                    hover.push_str("\nMore than the GPU has: the start will likely fail with out of memory.");
                }
                ui.label(RichText::new(text).small().color(color)).on_hover_text(hover);
            });
        }
        widgets::prop_row(ui, "Vision", |ui| {
            let r = ui.checkbox(&mut cfg.mmproj_cpu, "Projector on CPU").on_hover_text(
                "BONSAI_MMPROJ_CPU=1: keeps the vision projector in system RAM.\n\
                     Frees ~0.9 GB of VRAM; reading images gets slower, text is not affected.",
            );
            save |= r.changed();
        });
        widgets::prop_row(ui, "Start timeout", |ui| {
            let r = ui
                .add(egui::DragValue::new(&mut cfg.start_timeout_secs).range(30..=1800).speed(1.0).suffix(" s"))
                .on_hover_text("How long a request waits for Bonsai to start and load the model");
            save |= committed(&r);
        });

        ui.add_space(4.0);
        widgets::section_label(ui, "Extra environment");
        let mut remove = None;
        let ctx_set = cfg.ctx_size > 0;
        let mmproj_set = cfg.mmproj_cpu;
        for (i, var) in cfg.extra_env.iter_mut().enumerate() {
            ui.push_id(("srv-env", i), |ui| {
                // Right to left, so the row never gets wider than the panel (that would widen it);
                // inside `horizontal`, so it is one row high.
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, icons::REMOVE, "Remove this variable").clicked() {
                            remove = Some(i);
                        }
                        let spacing = ui.spacing().item_spacing.x;
                        // `desired_width` excludes the text edit's 4 px side margins.
                        let field = ((ui.available_width() - ENV_EQ_W - 2.0 * spacing) / 2.0 - 9.0).floor().max(30.0);
                        let value = ui.add(
                            egui::TextEdit::singleline(&mut var.value)
                                .hint_text("value")
                                .font(egui::TextStyle::Monospace)
                                .desired_width(field),
                        );
                        ui.add_sized([ENV_EQ_W, 20.0], egui::Label::new(RichText::new("=").color(Palette::TEXT_DIM)));
                        let key = ui.add(
                            egui::TextEdit::singleline(&mut var.key)
                                .hint_text("NAME")
                                .font(egui::TextStyle::Monospace)
                                .desired_width(field),
                        );
                        save |= key.changed() || value.changed();
                        // "BONSAI_KV4=1" typed into the name field (as the hint suggests): split it
                        // once the field is left (while typing, the value would land in the name).
                        save |= key.lost_focus() && split_env_assignment(var);
                    })
                });
                let key = var.key.trim();
                if bad_env_name(key) {
                    ui.label(RichText::new("Names cannot contain spaces or =").small().color(Palette::ERR));
                } else if (key == "BONSAI_CTX" && ctx_set) || (key == "BONSAI_MMPROJ_CPU" && mmproj_set) {
                    ui.label(RichText::new("Also set above - this value wins").small().color(Palette::WARN));
                }
            });
        }
        if let Some(i) = remove {
            cfg.extra_env.remove(i);
            save = true;
        }
        let add = ui
            .button(format!("{} Add variable", icons::ADD))
            .on_hover_text("Pass another environment variable to the launcher, e.g. BONSAI_KV4=1 or BONSAI_NGL=0");
        if add.clicked() {
            cfg.extra_env.push(EnvVar::default());
            save = true;
        }

        let env = cfg.launcher_env();
        ui.add_space(4.0);
        // One label, so the two fonts share a baseline and long lists wrap.
        let mut job = LayoutJob::default();
        let text_format = |font_id: FontId, color: Color32| TextFormat { font_id, color, ..Default::default() };
        job.append("Launcher gets  ", 0.0, text_format(FontId::proportional(11.0), Palette::TEXT_DIM));
        if env.is_empty() {
            job.append("nothing extra", 0.0, text_format(FontId::monospace(11.0), Palette::TEXT));
        }
        for (i, (k, v)) in env.iter().enumerate() {
            let color = if bad_env_name(k) { Palette::ERR } else { Palette::TEXT };
            let gap = if i > 0 { "  " } else { "" };
            job.append(&format!("{gap}{k}={v}"), 0.0, text_format(FontId::monospace(11.0), color));
        }
        let mut hover = "Environment variables passed to the launcher on the next start".to_string();
        if env.iter().any(|(k, _)| bad_env_name(k)) {
            hover.push_str("\nRed: not a valid name - the launcher does not get that variable as typed");
        }
        ui.add(egui::Label::new(job).wrap()).on_hover_text(hover);

        ui.add_space(2.0);
        widgets::thin_separator(ui);
        ui.add_space(2.0);
        // The running server got other settings than the launcher would get now.
        let changed = run_env.is_some_and(|run_env| run_env != env);
        ui.horizontal(|ui| {
            let (text, color) = if changed {
                ("Changed - restart Bonsai to apply", Palette::WARN)
            } else {
                ("Context, vision and env apply on the next start", Palette::TEXT_DIM)
            };
            ui.label(RichText::new(icons::INFO).color(color));
            ui.add(egui::Label::new(RichText::new(text).small().color(color)).wrap());
        });
        if running {
            let restart = ui
                .add_enabled(
                    can_restart,
                    egui::Button::new(format!("{} Restart now", icons::RESTART))
                        .min_size(egui::vec2(ui.available_width(), 22.0)),
                )
                .on_hover_text("Stop and start Bonsai with these options (a running request is cancelled)")
                .on_disabled_hover_text("Bonsai is still starting");
            if restart.clicked() {
                actions.push(Action::Restart);
            }
        }
        if save {
            actions.push(Action::SaveConfig);
        }
    });
}

/// A name the launcher cannot get as typed: Windows reads `A=B` + `C` as `A` = `B=C`.
fn bad_env_name(key: &str) -> bool {
    let key = key.trim();
    key.contains(char::is_whitespace) || key.contains('=')
}

/// `NAME=value` in the name field -> name `NAME`, value `value` (an empty part after `=` keeps the
/// value). Returns whether it changed anything.
fn split_env_assignment(var: &mut EnvVar) -> bool {
    let Some((name, value)) = var.key.split_once('=') else { return false };
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return false; // "=x": nothing to split off; the red note stays
    }
    if !value.is_empty() {
        var.value = value.to_string();
    }
    var.key = name.to_string();
    true
}

/// VRAM the launcher's settings need on the GPU, in MB (an estimate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VramNeed {
    model: u64,
    projector: u64,
    context: u64,
    total: u64,
    /// `BONSAI_CTX` unset or 0: the running server's context, else the launcher's 16k guess.
    auto_ctx: bool,
    /// `BONSAI_KV4=1`: q4_0 KV cache.
    kv4: bool,
    /// `BONSAI_NGL` is set: layers may stay in system RAM, so `total` is the most it needs.
    ngl_set: bool,
}

/// What the launcher sees for `key`: the last entry wins (as with repeated `Command::env`).
fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The VRAM estimate for `env` (`AppConfig::launcher_env`), read the way start_llama_server.ps1
/// reads it. `weights` = (model, projector) MB.
fn vram_need(weights: (u64, u64), env: &[(String, String)], running_ctx: Option<u32>) -> VramNeed {
    let set_ctx = env_value(env, "BONSAI_CTX").and_then(|v| v.trim().parse::<u32>().ok()).filter(|&n| n > 0);
    let kv4 = env_value(env, "BONSAI_KV4") == Some("1");
    let mmproj_cpu = env_value(env, "BONSAI_MMPROJ_CPU")
        .is_some_and(|v| ["1", "true", "yes", "on"].iter().any(|on| v.eq_ignore_ascii_case(on)));
    let ngl_set = env_value(env, "BONSAI_NGL").is_some_and(|v| !v.is_empty());
    let fp16 = kv_mb(set_ctx.or(running_ctx).unwrap_or(LAUNCHER_DEFAULT_CTX));
    // q4_0 keeps 32 values in 18 bytes, FP16 in 64.
    let context = if kv4 { fp16 * 18 / 64 } else { fp16 };
    let projector = if mmproj_cpu { 0 } else { weights.1 };
    VramNeed {
        model: weights.0,
        projector,
        context,
        total: weights.0 + projector + context + VRAM_OVERHEAD_MB,
        auto_ctx: set_ctx.is_none(),
        kv4,
        ngl_set,
    }
}

/// A drag value commits when it was typed in or when the drag ends (not on every drag frame).
fn committed(r: &egui::Response) -> bool {
    (r.changed() && !r.dragged()) || r.drag_stopped()
}

fn ctx_label(ctx: u32) -> String {
    match ctx {
        0 => "Auto (launcher)".into(),
        n if n.is_multiple_of(1024) => format!("{} ({}k)", fmt_int(u64::from(n)), n / 1024),
        n => format!("{} (custom)", fmt_int(u64::from(n))),
    }
}

/// KV cache in MB for `ctx` tokens.
fn kv_mb(ctx: u32) -> u64 {
    u64::from(ctx) * KV_KIB_PER_TOKEN / 1024
}

// ---------------------------------------------------------------------------------------------
// Centre: status + GPU cards

fn cards_area(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let url = app.config.base_url();
    let top = ui.cursor().top();
    widgets::area_header(ui, "Bonsai server", |ui| {
        ui.label(RichText::new(&url).monospace().size(11.5).color(Palette::TEXT_DIM))
            .on_hover_text("Server address (Settings > Server URL)");
    });
    let header_h = ui.cursor().top() - top;
    let targets = app.server_ws.card_heights;
    let target = targets[0].max(targets[1]);
    let mut heights = [0.0f32; 2];
    let mut content_h = 0.0;
    egui::ScrollArea::vertical().id_salt("srv-cards-scroll").auto_shrink([false, false]).show(ui, |ui| {
        content_h = egui::Frame::new()
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                if ui.available_width() >= TWO_CARDS_MIN_W {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.columns(2, |cols| {
                        heights[0] = card(&mut cols[0], target, |ui| status_card(app, ui, actions));
                        heights[1] = card(&mut cols[1], target, |ui| gpu_card(app, ui));
                    });
                } else {
                    card(ui, 0.0, |ui| status_card(app, ui, actions));
                    ui.add_space(8.0);
                    card(ui, 0.0, |ui| gpu_card(app, ui));
                }
            })
            .response
            .rect
            .height();
    });
    app.server_ws.card_heights = heights;
    // + the panel frame's 2 px bottom gap and 1 px against rounding.
    app.server_ws.cards_area_h = header_h + content_h + 3.0;
}

/// Card frame; stretched to `target_h` (the taller card's height last frame). Returns the
/// natural content height.
fn card(ui: &mut Ui, target_h: f32, add: impl FnOnce(&mut Ui)) -> f32 {
    egui::Frame::new()
        .fill(Palette::PANEL_BODY)
        .stroke(Stroke::new(1.0_f32, Palette::OUTLINE))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Before the content: `set_min_height` grows from the cursor.
            ui.set_min_height(target_h);
            let top = ui.cursor().top();
            add(ui);
            // The cursor sits one item spacing below the last widget.
            (ui.cursor().top() - top - ui.spacing().item_spacing.y).max(0.0)
        })
        .inner
}

fn is_running(state: &ServerState) -> bool {
    matches!(state, ServerState::Starting | ServerState::Loading | ServerState::Ready)
}

/// A llama-server.exe of the chosen install that the GPU lists while Bonsai is neither running nor
/// stopping (e.g. it stopped answering, or a stop failed): it still holds the VRAM, and Stop ends it.
/// A llama-server started from other files (another app, another port) is not ours to stop.
fn stray_server(app: &AppState) -> Option<&gpu::GpuProcess> {
    let state = &app.status.state;
    if is_running(state) || matches!(state, ServerState::Stopping) {
        return None;
    }
    let own_exe = app.status.install.as_ref().and_then(|i| i.server_exe.as_deref());
    app.status.gpu.as_ref()?.processes.iter().find(|p| gpu::is_llama_server(&p.name) && same_server(&p.name, own_exe))
}

/// `name` (nvidia-smi's process name: usually the full path) is the install's server exe. A bare file
/// name, or an install without a known exe, cannot be told apart and counts as ours.
fn same_server(name: &str, own_exe: Option<&std::path::Path>) -> bool {
    let (Some(own), true) = (own_exe, name.contains(['\\', '/'])) else { return true };
    let norm = |t: &str| t.trim().replace('/', "\\").to_lowercase();
    norm(name) == norm(&own.to_string_lossy())
}

fn state_color(state: &ServerState) -> Color32 {
    match state {
        ServerState::Ready => Palette::OK,
        ServerState::Starting | ServerState::Loading => Palette::INFO,
        ServerState::Stopping => Palette::WARN,
        ServerState::Stopped => Palette::TEXT_FAINT,
        ServerState::Failed(_) => Palette::ERR,
    }
}

fn state_title(state: &ServerState) -> &'static str {
    match state {
        ServerState::Ready => "Ready",
        ServerState::Starting => "Starting...",
        ServerState::Loading => "Loading model...",
        ServerState::Stopping => "Stopping...",
        ServerState::Stopped => "Stopped",
        ServerState::Failed(_) => "Start failed",
    }
}

/// One line under the big state: what it means for the user.
fn state_hint(app: &AppState) -> String {
    let s = &app.status;
    match &s.state {
        ServerState::Ready if s.busy || s.queued > 0 => {
            let queued = if s.queued > 0 { format!(", {} waiting", s.queued) } else { String::new() };
            format!("Working on a request{queued}")
        }
        ServerState::Ready => "Answers right away - write a prompt or chat".into(),
        ServerState::Starting => "The launcher is starting llama-server".into(),
        ServerState::Loading => "Loading the model into the GPU".into(),
        ServerState::Stopping => "Stopping llama-server - the GPU is free when it is done".into(),
        ServerState::Stopped if stray_server(app).is_some() => {
            "llama-server.exe still runs but does not answer - Stop ends it and frees the GPU".into()
        }
        ServerState::Stopped if !s.install_ok => "Bonsai is not set up yet - see Install (or Models)".into(),
        ServerState::Stopped if app.config.auto_start => {
            "Starts by itself when you write a prompt - or press Start".into()
        }
        ServerState::Stopped => "Auto-start is off - press Start before writing".into(),
        ServerState::Failed(_) => "See the log below for the details".into(),
    }
}

fn status_card(app: &AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let s = &app.status;
    let running = is_running(&s.state);
    let ready = s.state.is_ready();

    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 26.0), Sense::hover());
        let color = state_color(&s.state);
        ui.painter().circle_filled(rect.center(), 6.5, color);
        if ready {
            ui.painter().circle_stroke(rect.center(), 8.5, Stroke::new(1.0_f32, color.gamma_multiply(0.45)));
        }
        ui.label(RichText::new(state_title(&s.state)).size(20.0).strong().color(Palette::TEXT));
        if s.state.is_busy() {
            ui.spinner();
        }
        if running {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if s.started_by_us {
                    widgets::badge(ui, "started by this app", Palette::OK)
                        .on_hover_text("Idle auto-stop and stop-on-exit apply to it");
                } else {
                    widgets::badge(ui, "started outside the app", Palette::INFO).on_hover_text(
                        "Started with start_bonsai.bat or a terminal: this app uses it, but never stops it by itself",
                    );
                }
            });
        }
    });
    ui.add(egui::Label::new(RichText::new(state_hint(app)).color(Palette::TEXT_DIM)).wrap());
    if let ServerState::Failed(message) = &s.state {
        ui.add_space(4.0);
        callout(ui, Palette::ERR, icons::CROSS, message);
    }
    ui.add_space(6.0);

    let props = s.props.as_ref().filter(|_| ready);
    fact_row(ui, "Model", |ui| match props {
        Some(p) => {
            ui.add(egui::Label::new(&p.model_name).truncate()).on_hover_text(&p.model_path);
        }
        None => match s.install.as_ref().and_then(|i| i.primary_model()) {
            Some(model) => {
                ui.add(egui::Label::new(RichText::new(file_name(model)).color(Palette::TEXT_DIM)).truncate())
                    .on_hover_text(format!("{}\n(not loaded - the launcher picks this one)", model.display()));
            }
            None => {
                ui.label(RichText::new("-").color(Palette::TEXT_FAINT));
            }
        },
    });
    let dash = || RichText::new("-").color(Palette::TEXT_FAINT);
    fact_row(ui, "Type", |ui| match props.filter(|p| !p.model_ftype.is_empty()) {
        Some(p) => ui.add(egui::Label::new(&p.model_ftype).truncate()),
        None => ui.label(dash()),
    });
    fact_row(ui, "Context", |ui| match props {
        Some(p) => {
            let slots = if p.total_slots == 1 { "1 slot".to_string() } else { format!("{} slots", p.total_slots) };
            ui.label(format!("{} tokens · {slots}", fmt_int(u64::from(p.n_ctx))))
                .on_hover_text("Tokens per request (system prompt + request + answer); set in Launch options > Context")
        }
        None => ui.label(dash()),
    });
    fact_row(ui, "Vision", |ui| match props {
        Some(p) if p.vision => ui.label("Yes - images can be sent"),
        Some(_) => ui.label(RichText::new("No - no vision projector loaded").color(Palette::WARN)),
        None => ui.label(dash()),
    });
    fact_row(ui, "Build", |ui| match props.filter(|p| !p.build_info.is_empty()) {
        Some(p) => ui.label(format!("llama.cpp {}", p.build_info)),
        None => ui.label(dash()),
    });
    fact_row(ui, "Uptime", |ui| match (s.uptime_secs, running) {
        (Some(secs), true) => ui.label(widgets::fmt_secs(secs)),
        (None, true) => ui.label(RichText::new("unknown (started outside the app)").color(Palette::TEXT_DIM)),
        _ => ui.label(dash()),
    });
    fact_row(ui, "Idle", |ui| {
        if ready {
            ui.label(widgets::fmt_secs(s.idle_secs)).on_hover_text("Time since the last request")
        } else {
            ui.label(dash())
        }
    });
    let (auto_stop, auto_color) = auto_stop_text(app);
    fact_row(ui, "Auto-stop", |ui| ui.add(egui::Label::new(RichText::new(auto_stop).color(auto_color)).truncate()));

    ui.add_space(8.0);
    let can_start = matches!(s.state, ServerState::Stopped | ServerState::Failed(_)) && s.install_ok;
    let start_reason =
        if !s.install_ok { "Bonsai is not set up yet - see Install" } else { "Bonsai is running already" };
    // Also a llama-server.exe that does not answer: it holds the GPU until it is stopped.
    let stray = stray_server(app);
    let can_stop = running || stray.is_some();
    let (stop_hover, stop_off) = match (stray, &s.state) {
        (Some(p), _) => {
            (format!("End llama-server.exe (PID {}): it runs but does not answer, and holds the GPU", p.pid), "")
        }
        (None, ServerState::Stopping) => (String::new(), "Bonsai is stopping - the GPU is free when it is done"),
        (None, _) => (
            "Stop Bonsai (also one started outside this app); running and waiting requests are cancelled".into(),
            "Bonsai is not running - the GPU is free",
        ),
    };
    let accent = theme::accent(ui.ctx());
    equal_buttons(ui, 3, |ui, w| {
        let mut start = egui::Button::new(RichText::new(format!("{} Start", icons::PLAY)).color(if can_start {
            Color32::WHITE
        } else {
            Palette::TEXT
        }))
        .min_size(egui::vec2(w, 24.0));
        if can_start {
            start = start.fill(accent);
        }
        let start = ui
            .add_enabled(can_start, start)
            .on_hover_text("Start Bonsai now - also while other programs use the GPU")
            .on_disabled_hover_text(start_reason);
        if start.clicked() {
            actions.push(Action::Start);
        }
        let stop = ui
            .add_enabled(can_stop, egui::Button::new(format!("{} Stop", icons::STOP)).min_size(egui::vec2(w, 24.0)))
            .on_hover_text(stop_hover.as_str())
            .on_disabled_hover_text(stop_off);
        if stop.clicked() {
            actions.push(Action::Stop);
        }
        let restart = ui
            .add_enabled(
                matches!(s.state, ServerState::Ready | ServerState::Loading),
                egui::Button::new(format!("{} Restart", icons::RESTART)).min_size(egui::vec2(w, 24.0)),
            )
            .on_hover_text("Stop and start again (applies new launch options); a running request is cancelled")
            .on_disabled_hover_text("Bonsai is not running");
        if restart.clicked() {
            actions.push(Action::Restart);
        }
    });
    equal_buttons(ui, 2, |ui, w| {
        let web = ui
            .add_enabled(ready, egui::Button::new("🌐 Open web UI").min_size(egui::vec2(w, 24.0)))
            .on_hover_text(format!("Open llama-server's own chat page in the browser ({})", app.config.base_url()))
            .on_disabled_hover_text("Bonsai is not running");
        if web.clicked() {
            actions.push(Action::OpenWebUi);
        }
        let free = ui
            .add_enabled(
                can_stop,
                egui::Button::new(format!("{} Free GPU for ComfyUI", icons::POWER)).min_size(egui::vec2(w, 24.0)),
            )
            .on_hover_text(
                "Stop Bonsai so ComfyUI gets the GPU (Bonsai holds ~7.7 GB of the 8 GB).\n\
                 With auto-start on, it starts again when you write the next prompt.",
            )
            .on_disabled_hover_text(stop_off);
        if free.clicked() {
            actions.push(Action::Stop);
        }
    });
}

/// `n` buttons of equal width in one row; `add` gets the width of each.
fn equal_buttons(ui: &mut Ui, n: usize, add: impl FnOnce(&mut Ui, f32)) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let n = n.max(1) as f32;
        let w = ((ui.available_width() - 4.0 * (n - 1.0)) / n).floor().max(40.0);
        add(ui, w);
    });
}

fn auto_stop_text(app: &AppState) -> (String, Color32) {
    let s = &app.status;
    let minutes = app.config.auto_stop_idle_minutes;
    if !s.state.is_ready() {
        return ("-".into(), Palette::TEXT_FAINT);
    }
    if minutes == 0 {
        return ("Off (Launch options: 0 = never)".into(), Palette::TEXT_DIM);
    }
    if !s.started_by_us {
        return ("Only for a server this app started".into(), Palette::TEXT_DIM);
    }
    match s.auto_stop_in_secs {
        Some(left) => (
            format!("In {} (after {minutes} idle min)", widgets::fmt_secs(left)),
            if left < 60 { Palette::WARN } else { Palette::TEXT },
        ),
        None if s.busy || s.queued > 0 => ("Paused while a request runs".into(), Palette::TEXT_DIM),
        None => (format!("After {minutes} idle min"), Palette::TEXT_DIM),
    }
}

fn gpu_card(app: &AppState, ui: &mut Ui) {
    let Some(gpu) = app.status.gpu.as_ref() else {
        ui.label(RichText::new("GPU").strong());
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                RichText::new("No NVIDIA GPU information - nvidia-smi was not found or did not answer.")
                    .color(Palette::TEXT_DIM),
            )
            .wrap(),
        );
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("GPU").strong());
        ui.add(egui::Label::new(RichText::new(&gpu.name).color(Palette::TEXT_DIM)).truncate());
    });
    ui.add_space(4.0);

    let fraction = gpu.mem_fraction();
    let vram_color = if fraction >= 0.92 {
        Palette::ERR
    } else if fraction >= 0.75 {
        Palette::WARN
    } else {
        Palette::BLUE
    };
    fact_row(ui, "VRAM", |ui| {
        let text = format!(
            "{:.1} / {:.1} GB · {:.0} %",
            gpu.mem_used_mb as f32 / 1024.0,
            gpu.mem_total_mb as f32 / 1024.0,
            fraction * 100.0
        );
        usage_bar(ui, fraction, vram_color, &text).on_hover_text(format!(
            "{} of {} used{}",
            gpu::fmt_mb(gpu.mem_used_mb),
            gpu::fmt_mb(gpu.mem_total_mb),
            if fraction >= 0.92 { " - full: ComfyUI cannot render now" } else { "" }
        ));
    });
    fact_row(ui, "Load", |ui| {
        usage_bar(ui, gpu.util_pct as f32 / 100.0, Palette::BLUE, &format!("{} %", gpu.util_pct));
    });
    fact_row(ui, "Temp", |ui| {
        let color = if gpu.temp_c >= 87 {
            Palette::ERR
        } else if gpu.temp_c >= 78 {
            Palette::WARN
        } else {
            Palette::TEXT
        };
        ui.label(RichText::new(format!("{} °C", gpu.temp_c)).color(color));
    });

    ui.add_space(6.0);
    widgets::section_label(ui, "Processes on the GPU");
    if gpu.processes.is_empty() {
        ui.label(RichText::new("None").color(Palette::TEXT_DIM));
    } else {
        process_table(ui, gpu);
    }
    gpu_note(app, ui, gpu);
}

fn process_table(ui: &mut Ui, gpu: &GpuInfo) {
    let header = |ui: &mut Ui, text: &str| {
        ui.label(RichText::new(text).small().color(Palette::TEXT_DIM));
    };
    egui::Grid::new("srv-gpu-processes").num_columns(4).spacing([14.0, 3.0]).min_row_height(20.0).show(ui, |ui| {
        header(ui, "Name");
        header(ui, "PID");
        header(ui, "Memory");
        header(ui, "");
        ui.end_row();
        for p in &gpu.processes {
            let name = if p.name.chars().count() > PROCESS_NAME_CHARS {
                format!("{}…", p.name.chars().take(PROCESS_NAME_CHARS - 1).collect::<String>())
            } else {
                p.name.clone()
            };
            ui.label(name).on_hover_text(format!("{} (PID {})", gpu::describe_process(p), p.pid));
            ui.label(RichText::new(p.pid.to_string()).monospace().color(Palette::TEXT_DIM));
            if p.mem_mb > 0 {
                ui.label(gpu::fmt_mb(p.mem_mb));
            } else {
                ui.label(RichText::new("-").color(Palette::TEXT_FAINT))
                    .on_hover_text("Windows does not report VRAM per program");
            }
            if gpu::is_llama_server(&p.name) {
                widgets::badge(ui, "Bonsai", Palette::OK);
            } else if let Some(hint) = gpu::process_hint(&p.name) {
                widgets::badge(ui, &format!("{} {hint}", icons::WARN), Palette::WARN)
                    .on_hover_text("Uses the GPU next to Bonsai - both may run out of memory");
            } else {
                ui.label("");
            }
            ui.end_row();
        }
    });
}

/// What the GPU numbers mean for starting Bonsai or rendering in ComfyUI.
fn gpu_note(app: &AppState, ui: &mut Ui, gpu: &GpuInfo) {
    let others = gpu::other_processes(gpu);
    let running = is_running(&app.status.state);
    ui.add_space(6.0);
    if running && !others.is_empty() {
        let names: Vec<String> = others.iter().map(|p| p.name.clone()).collect();
        callout(
            ui,
            Palette::WARN,
            icons::WARN,
            &format!(
                "{} also use{} the GPU. With Bonsai loaded the memory is full - free the GPU before rendering in ComfyUI.",
                names.join(", "),
                if names.len() == 1 { "s" } else { "" }
            ),
        );
    } else if matches!(app.status.state, ServerState::Stopping) {
        // Not "free" yet: llama-server.exe holds the memory until it has exited.
        ui.label(
            RichText::new(format!("{} Stopping Bonsai - the GPU is free for ComfyUI once it has exited", icons::INFO))
                .color(Palette::WARN),
        );
    } else if let Some(p) = stray_server(app) {
        callout(
            ui,
            Palette::WARN,
            icons::WARN,
            &format!(
                "llama-server.exe (PID {}) still holds the GPU but does not answer. Stop ends it - until then the \
                 GPU is not free for ComfyUI.",
                p.pid
            ),
        );
    } else if !running {
        match gpu::vram_busy_message(gpu, app.config.vram_busy_mb) {
            Some(message) => {
                callout(ui, Palette::WARN, icons::WARN, &format!("{message} Auto-start waits; Start forces it."))
            }
            None => {
                ui.label(
                    RichText::new(format!("{} Bonsai is stopped - the GPU is free for ComfyUI", icons::CHECK))
                        .color(Palette::OK),
                );
            }
        }
    }
}

/// Flat bar with centred text (Blender's progress style); readable on every fill colour.
fn usage_bar(ui: &mut Ui, fraction: f32, color: Color32, text: &str) -> egui::Response {
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

/// Dim right-aligned label + value, sized for the cards.
fn fact_row<R>(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(FACT_LABEL_W, 20.0), Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(label).color(Palette::TEXT_DIM));
        });
        ui.add_space(4.0);
        add(ui)
    })
    .inner
}

// ---------------------------------------------------------------------------------------------
// Centre: log

fn log_area(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let log_file = log_path(app);
    let log_exists = file_exists(app, &log_file);
    let external_log = app
        .status
        .install
        .as_ref()
        .map(|i| i.root.join("server.log"))
        .filter(|_| app.status.state.is_ready() && !app.status.started_by_us);
    let external_log = external_log.filter(|p| file_exists(app, p));

    let filter = app.server_ws.log_filter.trim().to_lowercase();
    let lines = &app.status.log_tail;
    let shown: Vec<&String> =
        lines.iter().filter(|line| filter.is_empty() || line.to_lowercase().contains(&filter)).collect();
    let total = lines.len();
    let st = &mut app.server_ws;
    let mut view = st.view;

    bottom_header(ui, &mut view, |ui| {
        // Right to left: the rightmost button comes first.
        let open = ui
            .add_enabled(log_exists, egui::Button::new(format!("{} Open log file", icons::FOLDER)))
            .on_hover_text(format!("Open {} in the default editor", log_file.display()))
            .on_disabled_hover_text("No log file yet - it is written when this app starts Bonsai");
        if open.clicked() {
            actions.push(Action::OpenLog);
        }
        let copy = ui
            .add_enabled(!shown.is_empty(), egui::Button::new(format!("{} Copy", icons::COPY)))
            .on_hover_text("Copy the lines shown (filtered) to the clipboard")
            .on_disabled_hover_text("Nothing to copy");
        if copy.clicked() {
            actions.push(Action::CopyLog(shown.iter().map(|l| (*l).clone()).collect()));
        }
        ui.separator();
        if ui.add(egui::Button::new("Wrap").selected(st.wrap)).on_hover_text("Wrap long lines").clicked() {
            st.wrap = !st.wrap;
        }
        let autoscroll = ui
            .add(egui::Button::new("Autoscroll").selected(st.autoscroll))
            .on_hover_text("Keep the newest line in view");
        if autoscroll.clicked() {
            st.autoscroll = !st.autoscroll;
            st.jump_to_end = st.autoscroll;
        }
        ui.separator();
        if !st.log_filter.is_empty() && widgets::icon_button(ui, "✖", "Clear the filter").clicked() {
            st.log_filter.clear();
        }
        let filter_edit = ui.add(
            egui::TextEdit::singleline(&mut st.log_filter)
                .id_salt("srv-log-filter")
                .hint_text(format!("{} Filter", icons::SEARCH))
                // Leaves room for the line count next to the title in a narrow window.
                .desired_width((ui.available_width() - 110.0).clamp(70.0, 170.0)),
        );
        // egui drops a text edit's focus on Esc before the frame runs: it has just lost it.
        if filter_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            st.log_filter.clear();
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let count = if filter.is_empty() {
                format!("last {} line{}", total, if total == 1 { "" } else { "s" })
            } else {
                format!("{} of {} lines", shown.len(), total)
            };
            if ui.available_width() > 90.0 {
                ui.label(RichText::new(count).small().color(Palette::TEXT_DIM));
            }
        });
    });
    st.view = view;

    if let Some(path) = &external_log {
        egui::Frame::new().inner_margin(Margin::symmetric(8, 3)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icons::INFO).color(Palette::INFO));
                ui.label(
                    RichText::new("Bonsai was started outside this app - its output goes to its own log.")
                        .small()
                        .color(Palette::TEXT_DIM),
                );
                let open = ui.small_button(format!("Open {}", path.display())).on_hover_text("Open that log file");
                if open.clicked() {
                    actions.push(Action::OpenPath(path.clone()));
                }
            });
        });
    }

    let mono = FontId::monospace(LOG_FONT);
    egui::Frame::new().fill(Palette::INPUT).inner_margin(Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        if shown.is_empty() {
            ui.centered_and_justified(|ui| {
                let text = if total == 0 {
                    "No log yet - Bonsai's output appears here when this app starts it."
                } else {
                    "No line matches the filter."
                };
                ui.label(RichText::new(text).color(Palette::TEXT_DIM));
            });
            return;
        }
        let scroll = if st.wrap { egui::ScrollArea::vertical() } else { egui::ScrollArea::both() };
        scroll.id_salt("srv-log-scroll").auto_shrink([false, false]).stick_to_bottom(st.autoscroll).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for line in &shown {
                let label = egui::Label::new(log_line_job(line, &mono));
                ui.add(if st.wrap { label.wrap() } else { label.extend() });
            }
            if st.jump_to_end {
                ui.scroll_to_cursor(Some(Align::BOTTOM));
                st.jump_to_end = false;
            }
        });
    });
}

/// Header strip of the bottom area: Log / Models tabs on the left (Blender editor tabs), then
/// `add_right` laid out right-to-left.
fn bottom_header(ui: &mut Ui, view: &mut BottomView, add_right: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(Palette::HEADER).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 1.0;
            for (tab, label, tip) in [
                (BottomView::Log, format!("{} Log", icons::NOTE), "Bonsai's server log"),
                (
                    BottomView::Models,
                    format!("{} Models", icons::LIBRARY),
                    "Models and server builds: download, use, delete",
                ),
            ] {
                let selected = *view == tab;
                let button = egui::Button::new(RichText::new(label).color(if selected {
                    Color32::WHITE
                } else {
                    Palette::TEXT
                }))
                .fill(if selected { Palette::BLUE } else { Palette::WIDGET })
                .min_size(egui::vec2(78.0, 20.0));
                if ui.add(button).on_hover_text(tip).clicked() {
                    *view = tab;
                }
            }
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.with_layout(Layout::right_to_left(Align::Center), add_right);
        });
    });
}

/// How a log line is coloured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineKind {
    Normal,
    Good,
    Warn,
    Error,
    /// This app's `=== <time> start ===` / `[bh3] ...` lines.
    App,
}

fn line_kind(line: &str) -> LineKind {
    let trimmed = line.trim_start();
    if trimmed.starts_with("===") || trimmed.starts_with("[bh3]") {
        return LineKind::App;
    }
    // llama.cpp: "<time> <level> <module> ...", level I / W / E / D.
    if let Some((_, level)) = llama_prefix(trimmed) {
        match level {
            'E' => return LineKind::Error,
            'W' => return LineKind::Warn,
            _ => {}
        }
    }
    let lower = trimmed.to_ascii_lowercase();
    const ERROR_WORDS: [&str; 7] = ["error", "fail", "exception", "panic", "out of memory", "cannot", "abort"];
    if ERROR_WORDS.iter().any(|w| lower.contains(w)) {
        LineKind::Error
    } else if lower.contains("warn") {
        LineKind::Warn
    } else if lower.contains("model loaded") || lower.contains("listening on") {
        LineKind::Good
    } else {
        LineKind::Normal
    }
}

/// Byte length of a llama.cpp `<time> <level> ` prefix and the level letter.
fn llama_prefix(line: &str) -> Option<(usize, char)> {
    let time_end = line.find(' ')?;
    let time = &line[..time_end];
    if time.is_empty() || !time.contains('.') || !time.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let mut rest = line[time_end + 1..].chars();
    let level = rest.next()?;
    if !matches!(level, 'I' | 'W' | 'E' | 'D') || rest.next() != Some(' ') {
        return None;
    }
    Some((time_end + 3, level))
}

/// A log line with a faint llama.cpp timestamp and a colour by severity.
fn log_line_job(line: &str, font: &FontId) -> LayoutJob {
    let color = match line_kind(line) {
        LineKind::Normal => Palette::TEXT,
        LineKind::Good => Palette::OK,
        LineKind::Warn => Palette::WARN,
        LineKind::Error => Palette::ERR,
        LineKind::App => Palette::INFO,
    };
    let format = |color: Color32| TextFormat { font_id: font.clone(), color, ..Default::default() };
    let mut job = LayoutJob::default();
    let line = line.trim_end();
    match llama_prefix(line) {
        Some((end, _)) if end <= line.len() => {
            job.append(&line[..end], 0.0, format(Palette::TEXT_FAINT));
            job.append(&line[end..], 0.0, format(color));
        }
        _ => job.append(line, 0.0, format(color)),
    }
    job
}

// ---------------------------------------------------------------------------------------------
// Cached file facts

fn log_path(app: &mut AppState) -> PathBuf {
    app.server_ws.log_path.get_or_insert_with(bh3::server::default_log_path).clone()
}

/// `path.is_file()`, re-checked at most every [`FILE_CHECK_SECS`].
fn file_exists(app: &mut AppState, path: &Path) -> bool {
    let now = app.time;
    let checks = &mut app.server_ws.file_checks;
    match checks.get(path) {
        Some(&(at, exists)) if now - at < FILE_CHECK_SECS => exists,
        _ => {
            let exists = path.is_file();
            checks.insert(path.to_path_buf(), (now, exists));
            exists
        }
    }
}

/// Sizes of the install's models and projector (read once per path).
fn model_sizes(app: &mut AppState, install: &BonsaiInstall) -> HashMap<PathBuf, Option<u64>> {
    let sizes = &mut app.server_ws.sizes;
    for path in install.models.iter().chain(install.mmproj.iter()) {
        sizes.entry(path.clone()).or_insert_with(|| std::fs::metadata(path).ok().map(|m| m.len()));
    }
    sizes.clone()
}

/// MB of the launcher's model and of the projector, for the VRAM estimate.
fn weights_mb(app: &mut AppState, install: &BonsaiInstall) -> (u64, u64) {
    let sizes = model_sizes(app, install);
    let mb =
        |p: Option<&PathBuf>| p.and_then(|p| sizes.get(p).copied().flatten()).map(|b| b / (1024 * 1024)).unwrap_or(0);
    (mb(install.primary_model()), mb(install.mmproj.as_ref()))
}

/// Why `dir` is not a usable install (checked once per folder).
fn folder_error(app: &mut AppState, dir: &Path) -> String {
    let st = &mut app.server_ws;
    if let Some((checked, error)) = &st.folder_error {
        if checked == dir {
            return error.clone();
        }
    }
    let error = match BonsaiInstall::detect(dir) {
        Ok(_) => "Checking the folder...".to_string(),
        Err(e) => format!("{e:#}"),
    };
    st.folder_error = Some((dir.to_path_buf(), error.clone()));
    error
}

// ---------------------------------------------------------------------------------------------
// Small helpers

/// Coloured box with an icon and wrapped text.
fn callout(ui: &mut Ui, color: Color32, icon: &str, text: &str) {
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

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// MB (MiB) as "5.5 GB" / "600 MB".
fn fmt_mb(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

fn fmt_bytes(bytes: u64) -> String {
    fmt_mb(bytes / (1024 * 1024))
}

/// `16384` -> `16,384`.
fn fmt_int(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_installs_server_counts_as_stray() {
        let own = std::path::Path::new(r"C:\App\runtime\bin\llama-server.exe");
        assert!(same_server(r"c:\app\runtime\bin\LLAMA-SERVER.EXE", Some(own)));
        assert!(!same_server(r"D:\Bonsai-demo\bin\cuda\llama-server.exe", Some(own)));
        assert!(same_server("llama-server.exe", Some(own)));
        assert!(same_server(r"D:\x\llama-server.exe", None));
    }

    #[test]
    fn log_lines_by_severity() {
        assert_eq!(line_kind("0.00.974.320 W srv  llama_server: CORS is set"), LineKind::Warn);
        assert_eq!(line_kind("0.02.137.466 W common_fit_params: failed to fit params"), LineKind::Warn);
        assert_eq!(line_kind("1.02.000.001 E srv  load_model: failed to load model"), LineKind::Error);
        assert_eq!(line_kind("0.18.449.416 I srv  llama_server: model loaded"), LineKind::Good);
        assert_eq!(line_kind("0.18.449.489 I srv  init: chat template ok"), LineKind::Normal);
        assert_eq!(line_kind("ggml_cuda_init: out of memory"), LineKind::Error);
        assert_eq!(line_kind("[WARN] Automatic drafting is not configured"), LineKind::Warn);
        assert_eq!(line_kind("=== 2026-10-28 10:41:00 start ==="), LineKind::App);
        assert_eq!(llama_prefix("0.00.974.320 W srv x"), Some((15, 'W')));
        assert_eq!(llama_prefix("Model: x"), None);
        assert_eq!(llama_prefix("12 Ex"), None);
    }

    #[test]
    fn context_labels_and_memory() {
        assert_eq!(ctx_label(0), "Auto (launcher)");
        assert_eq!(ctx_label(16384), "16,384 (16k)");
        assert_eq!(ctx_label(12000), "12,000 (custom)");
        assert_eq!(kv_mb(16384), 1024);
        assert_eq!(fmt_mb(5671), "5.5 GB");
        assert_eq!(fmt_mb(600), "600 MB");
    }

    #[test]
    fn vram_estimate_reads_the_launcher_env() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let weights = (5600, 600);
        // Auto: the running server's context, else the launcher's 16k guess.
        let auto = vram_need(weights, &[], None);
        assert_eq!((auto.context, auto.auto_ctx, auto.kv4, auto.ngl_set), (1024, true, false, false));
        assert_eq!(auto.total, 5600 + 600 + 1024 + VRAM_OVERHEAD_MB);
        assert_eq!(vram_need(weights, &[], Some(8192)).context, 512);
        // 32k with the q4_0 KV cache fits an 8 GB card; with FP16 it does not.
        let kv4 = vram_need(weights, &env(&[("BONSAI_CTX", "32768"), ("BONSAI_KV4", "1")]), None);
        assert_eq!((kv4.context, kv4.auto_ctx, kv4.kv4), (576, false, true));
        assert!(kv4.total < 8192);
        assert!(vram_need(weights, &env(&[("BONSAI_CTX", "32768")]), None).total > 8192);
        // The last value wins (extra env comes after the options); projector on CPU; BONSAI_NGL.
        let mixed = [("BONSAI_CTX", "16384"), ("BONSAI_MMPROJ_CPU", "1"), ("BONSAI_CTX", "8192"), ("BONSAI_NGL", "20")];
        let need = vram_need(weights, &env(&mixed), None);
        assert_eq!((need.context, need.projector, need.ngl_set), (512, 0, true));
        // As the launcher reads them: BONSAI_CTX=0 is auto, only BONSAI_KV4=1 turns q4_0 on.
        let other = vram_need(weights, &env(&[("BONSAI_CTX", "0"), ("BONSAI_KV4", "true")]), None);
        assert_eq!((other.auto_ctx, other.kv4), (true, false));
    }

    #[test]
    fn env_names_and_assignments() {
        assert!(bad_env_name("BONSAI_KV4=1"));
        assert!(bad_env_name("BONSAI KV4"));
        assert!(!bad_env_name(" BONSAI_KV4 "));
        let mut var = EnvVar { key: "BONSAI_KV4=1".into(), value: String::new() };
        assert!(split_env_assignment(&mut var));
        assert_eq!((var.key.as_str(), var.value.as_str()), ("BONSAI_KV4", "1"));
        // An empty part after "=" keeps the value; values may hold "=".
        let mut var = EnvVar { key: "BONSAI_NGL =".into(), value: "0".into() };
        assert!(split_env_assignment(&mut var));
        assert_eq!((var.key.as_str(), var.value.as_str()), ("BONSAI_NGL", "0"));
        let mut var = EnvVar { key: "OPTS=a=b".into(), value: String::new() };
        assert!(split_env_assignment(&mut var));
        assert_eq!((var.key.as_str(), var.value.as_str()), ("OPTS", "a=b"));
        // "=x" and plain names stay as they are.
        let mut var = EnvVar { key: "=x".into(), value: String::new() };
        assert!(!split_env_assignment(&mut var));
        let mut var = EnvVar { key: "BONSAI_KV4".into(), value: "1".into() };
        assert!(!split_env_assignment(&mut var));
    }

    #[test]
    fn runs_told_apart_by_start_estimate() {
        assert!(same_run(Some(100.0), Some(102.4)));
        assert!(same_run(None, None));
        assert!(!same_run(Some(100.0), Some(130.0)));
        assert!(!same_run(Some(100.0), None));
        assert!(!same_run(None, Some(100.0)));
    }
}
