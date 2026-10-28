//! `bh3 models ...` and `bh3 runtime ...`: the app's own model and server files, so Bonsai runs
//! without the Bonsai-demo folder (see [`crate::runtime`]).
//!
//! - Model files chosen with `models use <file.gguf>` stay where they are; only the path is saved
//!   (`runtime.model`, `runtime.mmproj`, `runtime.server_exe` in the config).
//! - Downloads print progress on stderr; the first Ctrl+C cancels and keeps the `.part` file, so
//!   the same command continues where it stopped (exit 130).

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Args, Subcommand};
use serde::Serialize;

use super::{
    absolute, config, console, eprint_line, load_config_strict, print_json, print_line, CliError, CliResult, Ctx,
    InstallJson, Source, CANCEL, EXIT_OK,
};
use crate::config::AppConfig;
use crate::runtime::{
    self, Backend, BackendChoice, CatalogModel, ImportMode, InstalledBuild, InstalledModel, ModelSource, RuntimeTask,
    TaskOutcome, TaskProgress,
};
use crate::server::BonsaiInstall;

#[derive(Debug, Subcommand)]
pub(super) enum ModelsCmd {
    /// Models on this PC (the app's models folder and the one in use) and the catalog
    List,
    /// Download a catalog model (see `bh3 models list`) or hf:<owner>/<repo>/<file.gguf>
    Download(DownloadArgs),
    /// Copy (or --move) the server build, model and projector of a Bonsai-demo folder into the app, then use them
    Import(ImportArgs),
    /// Use a model: an installed id, or any .gguf on this PC (used where it is - nothing is copied); `none` = back to bonsai_dir
    Use(UseArgs),
    /// Delete a downloaded or imported model (only files in the app's models folder)
    Remove(RemoveArgs),
}

#[derive(Debug, Args)]
pub(super) struct DownloadArgs {
    /// Catalog id (27b-ptq1, 27b-pq2, 9b-ptq1, 4b-ptq1, 2b-ptq1) or hf:<owner>/<repo>/<file.gguf>
    #[arg(value_name = "ID|hf:REPO/FILE")]
    spec: String,
    /// Vision projector file of the same repo [default: the catalog's; none for others]; `none` = no projector
    #[arg(long, value_name = "FILE")]
    mmproj: Option<String>,
    /// Use the model when the download is done
    #[arg(long = "use")]
    use_it: bool,
}

#[derive(Debug, Args)]
pub(super) struct ImportArgs {
    /// The Bonsai-demo folder (read: bin\<backend>\, models\..., scripts\webui-config.json)
    #[arg(value_name = "BONSAI_DEMO_DIR")]
    dir: PathBuf,
    /// Move the files instead of copying them (the Bonsai-demo folder can no longer start them)
    #[arg(long = "move")]
    move_files: bool,
    /// Only import; keep the current settings
    #[arg(long)]
    no_use: bool,
}

#[derive(Debug, Args)]
pub(super) struct UseArgs {
    /// Model id from `bh3 models list`, a path to a .gguf (used where it is), or `none`
    #[arg(value_name = "ID|FILE.gguf|none")]
    model: String,
    /// Vision projector (*mmproj*.gguf) [default: the one next to the model]; `none` = no image input
    #[arg(long, value_name = "FILE|none")]
    mmproj: Option<String>,
    /// llama-server.exe to start [default: the server build in the app, else the one of bonsai_dir]
    #[arg(long, value_name = "EXE")]
    server: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(super) struct RemoveArgs {
    /// Model id from `bh3 models list`
    #[arg(value_name = "ID")]
    id: String,
    /// Also when it is the model in use (the setting is cleared)
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum RuntimeCmd {
    /// The server build in use, the builds in the app, which build suits this PC, storage and free space
    Show,
    /// Download the llama-server build (PrismML's llama.cpp fork) and use it
    Download(RuntimeDownloadArgs),
}

#[derive(Debug, Args)]
pub(super) struct RuntimeDownloadArgs {
    /// Build: cuda-13.3, cuda-12.4, vulkan or cpu [default: picked from nvidia-smi]
    #[arg(long, value_name = "BACKEND", value_parser = parse_backend)]
    backend: Option<Backend>,
    /// Only download; keep the current settings
    #[arg(long)]
    no_use: bool,
}

fn parse_backend(text: &str) -> Result<Backend, String> {
    Backend::from_id(text).ok_or_else(|| format!("use cuda-13.3, cuda-12.4, vulkan or cpu (not '{text}')"))
}

// ---------------------------------------------------------------------------------------------

pub(super) fn cmd_models(ctx: &Ctx, cmd: &ModelsCmd) -> CliResult {
    match cmd {
        ModelsCmd::List => models_list(ctx),
        ModelsCmd::Download(args) => models_download(ctx, args),
        ModelsCmd::Import(args) => models_import(ctx, args),
        ModelsCmd::Use(args) => models_use(ctx, args),
        ModelsCmd::Remove(args) => models_remove(ctx, args),
    }
}

pub(super) fn cmd_runtime(ctx: &Ctx, cmd: &RuntimeCmd) -> CliResult {
    match cmd {
        RuntimeCmd::Show => runtime_show(ctx),
        RuntimeCmd::Download(args) => runtime_download(ctx, args),
    }
}

#[derive(Debug, Serialize)]
struct ModelJson {
    #[serde(flatten)]
    model: InstalledModel,
    active: bool,
}

#[derive(Debug, Serialize)]
struct CatalogJson {
    #[serde(flatten)]
    model: &'static CatalogModel,
    installed: bool,
    total_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ListJson {
    models_dir: PathBuf,
    free_bytes: Option<u64>,
    /// The model in use (`runtime.model`), also one outside the models folder.
    active_model: Option<PathBuf>,
    active_mmproj: Option<PathBuf>,
    installed: Vec<ModelJson>,
    catalog: Vec<CatalogJson>,
}

/// Models in the app's folder, plus the one in use when it lives elsewhere.
fn known_models(cfg: &AppConfig) -> Vec<InstalledModel> {
    let dir = runtime::models_dir(cfg);
    let mut models = runtime::scan_models(&dir);
    if let Some(active) = cfg.runtime.model.as_ref().filter(|m| m.is_file()) {
        if !models.iter().any(|m| runtime::same_file(&m.path, active)) {
            models.push(runtime::describe_model(active, Some(&dir)));
        }
    }
    models
}

fn is_active(cfg: &AppConfig, path: &Path) -> bool {
    cfg.runtime.model.as_deref().is_some_and(|m| runtime::same_file(m, path))
}

fn models_list(ctx: &Ctx) -> CliResult {
    let cfg = ctx.config();
    let dir = runtime::models_dir(cfg);
    let installed: Vec<ModelJson> =
        known_models(cfg).into_iter().map(|m| ModelJson { active: is_active(cfg, &m.path), model: m }).collect();
    let catalog = runtime::CATALOG
        .iter()
        .map(|m| CatalogJson {
            installed: installed.iter().any(|i| i.model.catalog_id == Some(m.id) && i.model.managed),
            total_bytes: m.total_bytes(),
            model: m,
        })
        .collect();
    let report = ListJson {
        free_bytes: runtime::free_space(&dir),
        models_dir: dir,
        active_model: cfg.runtime.model.clone(),
        active_mmproj: cfg.runtime.mmproj.clone(),
        installed,
        catalog,
    };
    if ctx.json {
        print_json(&report)?;
        return Ok(EXIT_OK);
    }
    let mut lines = vec![format!(
        "Models folder: {}{}",
        report.models_dir.display(),
        report.free_bytes.map(|f| format!(" ({} free)", runtime::fmt_bytes(f))).unwrap_or_default()
    )];
    if report.installed.is_empty() {
        lines.push("  no models yet: `bh3 models download 27b-ptq1` or `bh3 models use <file.gguf>`".into());
    }
    for m in &report.installed {
        let mark = if m.active { "*" } else { " " };
        let vision = match &m.model.mmproj {
            Some(p) => format!("+ {}", file_name(p)),
            None => "no projector".to_string(),
        };
        let place = if m.model.managed { String::new() } else { format!(" [in place: {}]", m.model.path.display()) };
        lines.push(format!("{mark} {:<52} {:>8}  {vision}{place}", m.model.id, runtime::fmt_bytes(m.model.bytes)));
    }
    lines.push(String::new());
    lines.push("Catalog (bh3 models download <ID>):".into());
    for c in &report.catalog {
        let state = if c.installed { "installed" } else { "" };
        lines.push(format!(
            "  {:<10} {:<36} {:>8}  {state}",
            c.model.id,
            c.model.title,
            runtime::fmt_bytes(c.total_bytes)
        ));
    }
    lines.push("Any other: bh3 models download hf:<owner>/<repo>/<file.gguf> [--mmproj <file>]".into());
    print_line(&lines.join("\n"));
    Ok(EXIT_OK)
}

/// Prints task progress on stderr: one updated line on a terminal, else a line every few seconds.
struct ProgressPrinter {
    quiet: bool,
    terminal: bool,
    last: Option<Instant>,
    last_file: String,
    width: usize,
}

impl ProgressPrinter {
    fn new(quiet: bool) -> Self {
        Self { quiet, terminal: std::io::stderr().is_terminal(), last: None, last_file: String::new(), width: 0 }
    }

    fn show(&mut self, p: &TaskProgress) {
        if self.quiet {
            return;
        }
        let new_file = p.file != self.last_file;
        let every = if self.terminal { 0.25 } else { 5.0 };
        if !new_file && self.last.is_some_and(|t| t.elapsed().as_secs_f64() < every) {
            return;
        }
        self.last = Some(Instant::now());
        let percent = if p.total > 0 { format!("{:>3.0}% ", p.fraction() * 100.0) } else { String::new() };
        let files = if p.files > 1 { format!(" ({}/{})", p.file_index, p.files) } else { String::new() };
        let line = format!("{} {}{files}: {percent}{}", p.phase, p.file, p.line());
        if self.terminal {
            if new_file && self.width > 0 {
                eprintln!();
            }
            let pad = self.width.saturating_sub(line.chars().count());
            eprint!("\r{line}{}", " ".repeat(pad));
            self.width = line.chars().count();
        } else {
            eprint_line(&line);
        }
        self.last_file = p.file.clone();
    }

    fn finish(&mut self) {
        if self.terminal && self.width > 0 {
            eprintln!();
            self.width = 0;
        }
    }
}

/// Run a task in the foreground with progress on stderr; Ctrl+C cancels (exit 130).
fn run_with_progress(ctx: &Ctx, task: &RuntimeTask, cfg: &AppConfig) -> Result<TaskOutcome, CliError> {
    console::install();
    ctx.note(format!("{}...", task.title()));
    let mut printer = ProgressPrinter::new(ctx.quiet);
    let result = runtime::run_task(task, cfg, &CANCEL, &mut |p| printer.show(p));
    printer.finish();
    match result {
        Ok(outcome) => {
            for note in &outcome.notes {
                ctx.warn(note);
            }
            Ok(outcome)
        }
        Err(e) if e.downcast_ref::<runtime::Cancelled>().is_some() || super::cancelled() => Err(CliError::cancelled(
            "cancelled (Ctrl+C): a partial download is kept - run the same command again to continue",
        )),
        Err(e) => Err(CliError::fail(format!("{e:#}"))),
    }
}

#[derive(Debug, Serialize)]
struct TaskJson {
    task: String,
    #[serde(flatten)]
    outcome: TaskOutcome,
    /// The settings now use the result.
    used: bool,
    install: Option<InstallJson>,
}

/// Save the config and describe the install it gives.
fn save_and_describe(cfg: &AppConfig) -> Result<BonsaiInstall, CliError> {
    cfg.save_to(&config::config_path()).map_err(CliError::from)?;
    Ok(BonsaiInstall::from_runtime(cfg))
}

fn print_task(ctx: &Ctx, report: &TaskJson, install: Option<&BonsaiInstall>) -> CliResult {
    if ctx.json {
        print_json(report)?;
    } else {
        let mut text = report.outcome.message.clone();
        if let Some(install) = install {
            text.push_str(&format!("\n{}", install.summary()));
        }
        print_line(&text);
    }
    if let Some(install) = install {
        for problem in install.problems() {
            ctx.warn(problem);
        }
    }
    Ok(EXIT_OK)
}

fn models_download(ctx: &Ctx, args: &DownloadArgs) -> CliResult {
    let source =
        ModelSource::parse(&args.spec, args.mmproj.as_deref()).map_err(|e| CliError::usage(format!("{e:#}")))?;
    let mut cfg = load_config_strict()?;
    let task = RuntimeTask::DownloadModel(source);
    let outcome = run_with_progress(ctx, &task, &cfg)?;
    let mut install = None;
    if args.use_it {
        cfg.runtime.model = outcome.model.clone();
        cfg.runtime.mmproj = outcome.mmproj.clone();
        install = Some(save_and_describe(&cfg)?);
    } else if let Some(model) = &outcome.model {
        ctx.note(format!("Use it with `bh3 models use \"{}\"`.", model.display()));
    }
    let report = TaskJson {
        task: task.title(),
        used: install.is_some(),
        install: install.as_ref().map(|i| InstallJson::of(i, Some(Source::Runtime))),
        outcome,
    };
    print_task(ctx, &report, install.as_ref())
}

fn models_import(ctx: &Ctx, args: &ImportArgs) -> CliResult {
    let dir = absolute(&args.dir);
    let mut cfg = load_config_strict()?;
    let mode = if args.move_files { ImportMode::Move } else { ImportMode::Copy };
    let plan = runtime::plan_import(&dir, &cfg).map_err(CliError::from)?;
    ctx.note(format!("{} file(s), {} in all.", plan.items.len(), runtime::fmt_bytes(plan.bytes)));
    let task = RuntimeTask::Import { dir, mode };
    let outcome = run_with_progress(ctx, &task, &cfg)?;
    let mut install = None;
    if !args.no_use {
        if outcome.server_exe.is_some() {
            cfg.runtime.server_exe = outcome.server_exe.clone();
        }
        if outcome.model.is_some() {
            cfg.runtime.model = outcome.model.clone();
            cfg.runtime.mmproj = outcome.mmproj.clone();
        }
        install = Some(save_and_describe(&cfg)?);
    }
    let report = TaskJson {
        task: task.title(),
        used: install.is_some(),
        install: install.as_ref().map(|i| InstallJson::of(i, Some(Source::Runtime))),
        outcome,
    };
    print_task(ctx, &report, install.as_ref())
}

#[derive(Debug, Serialize)]
struct UseJson {
    model: Option<PathBuf>,
    mmproj: Option<PathBuf>,
    server_exe: Option<PathBuf>,
    /// The Bonsai-demo folder is used again (`models use none`).
    bonsai_dir: Option<PathBuf>,
    install: Option<InstallJson>,
}

fn models_use(ctx: &Ctx, args: &UseArgs) -> CliResult {
    let mut cfg = load_config_strict()?;
    if let Some(server) = &args.server {
        let exe = absolute(server);
        if !exe.is_file() {
            return Err(CliError::usage(format!("{} does not exist", exe.display())));
        }
        cfg.runtime.server_exe = Some(exe);
    }
    if args.model.trim().eq_ignore_ascii_case("none") {
        cfg.runtime.model = None;
        cfg.runtime.mmproj = None;
        cfg.save_to(&config::config_path()).map_err(CliError::from)?;
        let report =
            UseJson { model: None, mmproj: None, server_exe: None, bonsai_dir: cfg.bonsai_dir.clone(), install: None };
        if ctx.json {
            print_json(&report)?;
        } else {
            print_line(&match &cfg.bonsai_dir {
                Some(dir) => format!("The app runtime is off: Bonsai starts from {} again.", dir.display()),
                None => "The app runtime is off (no Bonsai-demo folder is set either).".to_string(),
            });
        }
        return Ok(EXIT_OK);
    }
    let models = known_models(&cfg);
    let model = match runtime::find_model(&models, &args.model) {
        Some(m) => m.clone(),
        None => {
            let path = absolute(Path::new(args.model.trim()));
            if !path.is_file() {
                return Err(CliError::usage(format!(
                    "'{}' is neither a model of `bh3 models list` nor an existing .gguf file",
                    args.model
                )));
            }
            if !path.to_string_lossy().to_ascii_lowercase().ends_with(".gguf") {
                return Err(CliError::usage(format!("{} is not a .gguf file", path.display())));
            }
            runtime::describe_model(&path, Some(&runtime::models_dir(&cfg)))
        }
    };
    let mmproj = match args.mmproj.as_deref().map(str::trim) {
        Some(m) if m.eq_ignore_ascii_case("none") => None,
        Some(m) => {
            let path = absolute(Path::new(m));
            if !path.is_file() {
                return Err(CliError::usage(format!("{} does not exist", path.display())));
            }
            Some(path)
        }
        None => model.mmproj.clone(),
    };
    if mmproj.is_none() {
        ctx.warn(
            "no vision projector (*mmproj*.gguf) next to the model: image input is off (add one with --mmproj <file>)",
        );
    }
    cfg.runtime.model = Some(model.path.clone());
    cfg.runtime.mmproj = mmproj.clone();
    let install = save_and_describe(&cfg)?;
    if install.server_exe.is_none() {
        let choice = runtime::detect_backend();
        ctx.warn(format!(
            "no llama-server build yet: `bh3 runtime download` gets the {} build ({}); {}",
            choice.backend.label(),
            runtime::fmt_bytes(choice.backend.download_bytes()),
            choice.reason
        ));
    }
    let report = UseJson {
        model: Some(model.path.clone()),
        mmproj,
        server_exe: install.server_exe.clone(),
        bonsai_dir: None,
        install: Some(InstallJson::of(&install, Some(Source::Runtime))),
    };
    if ctx.json {
        print_json(&report)?;
    } else {
        let note = if model.managed { "" } else { "\n(The files stay where they are; only their paths are saved.)" };
        print_line(&format!("{}{note}\nApplies on the next start of Bonsai.", install.summary()));
    }
    Ok(EXIT_OK)
}

fn models_remove(ctx: &Ctx, args: &RemoveArgs) -> CliResult {
    let mut cfg = load_config_strict()?;
    let dir = runtime::models_dir(&cfg);
    let models = runtime::scan_models(&dir);
    let Some(model) = runtime::find_model(&models, &args.id) else {
        return Err(CliError::usage(format!(
            "no model '{}' in the app's models folder ({}) - see `bh3 models list`",
            args.id,
            dir.display()
        )));
    };
    let active = is_active(&cfg, &model.path);
    if active && !args.force {
        return Err(CliError::fail(format!(
            "{} is the model in use - choose another one first (`bh3 models use <ID>`), or pass --force",
            model.id
        )));
    }
    let removed = runtime::remove_model(model, &dir).map_err(CliError::from)?;
    if active {
        cfg.runtime.model = None;
        cfg.runtime.mmproj = None;
        cfg.save_to(&config::config_path()).map_err(CliError::from)?;
        ctx.warn("it was the model in use: no model is set now (`bh3 models use <ID>`)");
    }
    if ctx.json {
        print_json(&serde_json::json!({ "removed": removed, "was_active": active }))?;
    } else {
        print_line(&removed.iter().map(|p| format!("Deleted {}", p.display())).collect::<Vec<_>>().join("\n"));
    }
    Ok(EXIT_OK)
}

#[derive(Debug, Serialize)]
struct RuntimeJson {
    release_tag: &'static str,
    runtime_dir: PathBuf,
    models_dir: PathBuf,
    free_bytes: Option<u64>,
    /// The build for this PC.
    recommended: BackendChoice,
    recommended_bytes: u64,
    /// The app runtime is in use (a model is set).
    active: bool,
    server_exe: Option<PathBuf>,
    backend: Option<&'static str>,
    model: Option<PathBuf>,
    mmproj: Option<PathBuf>,
    builds: Vec<InstalledBuild>,
    problems: Vec<String>,
    warnings: Vec<String>,
}

fn runtime_show(ctx: &Ctx) -> CliResult {
    let cfg = ctx.config();
    let install = BonsaiInstall::from_runtime(cfg);
    let choice = runtime::detect_backend();
    let report = RuntimeJson {
        release_tag: runtime::RELEASE_TAG,
        runtime_dir: runtime::runtime_dir(cfg),
        models_dir: runtime::models_dir(cfg),
        free_bytes: runtime::free_space(&runtime::runtime_dir(cfg)),
        recommended_bytes: choice.backend.download_bytes(),
        recommended: choice,
        active: cfg.runtime.is_active(),
        backend: install.backend(),
        server_exe: install.server_exe.clone(),
        model: install.models.first().cloned(),
        mmproj: install.mmproj.clone(),
        builds: runtime::installed_builds(cfg),
        problems: install.problems(),
        warnings: install.warnings(),
    };
    if ctx.json {
        print_json(&report)?;
        return Ok(EXIT_OK);
    }
    let mut lines = vec![install.summary()];
    if !report.active {
        lines.push(match &cfg.bonsai_dir {
            Some(dir) => format!("(Not in use: no model is set, so Bonsai starts from {})", dir.display()),
            None => "(Not in use yet: `bh3 models use <file.gguf>` chooses a model)".to_string(),
        });
    }
    lines.push(format!(
        "Storage: {} ({} free); models in {}",
        report.runtime_dir.display(),
        report.free_bytes.map_or("?".to_string(), runtime::fmt_bytes),
        report.models_dir.display()
    ));
    lines.push(format!(
        "For this PC: the {} build, {} - {}",
        report.recommended.backend.label(),
        runtime::fmt_bytes(report.recommended_bytes),
        report.recommended.reason
    ));
    if report.builds.is_empty() {
        lines
            .push(format!("Server builds in the app: none (`bh3 runtime download`, release {})", runtime::RELEASE_TAG));
    } else {
        lines.push("Server builds in the app:".into());
        lines.extend(report.builds.iter().map(|b| format!("  {} {} - {}", b.tag, b.backend, b.dir.display())));
    }
    print_line(&lines.join("\n"));
    Ok(EXIT_OK)
}

fn runtime_download(ctx: &Ctx, args: &RuntimeDownloadArgs) -> CliResult {
    let mut cfg = load_config_strict()?;
    let backend = match args.backend {
        Some(b) => b,
        None => {
            let choice = runtime::detect_backend();
            ctx.note(format!("{} ({})", choice.reason, runtime::fmt_bytes(choice.backend.download_bytes())));
            choice.backend
        }
    };
    let task = RuntimeTask::DownloadServer(backend);
    let outcome = run_with_progress(ctx, &task, &cfg)?;
    let mut install = None;
    if !args.no_use {
        cfg.runtime.server_exe = outcome.server_exe.clone();
        install = Some(save_and_describe(&cfg)?);
        if !cfg.runtime.is_active() {
            ctx.note("Choose the model with `bh3 models use <file.gguf>` (or `bh3 models download 27b-ptq1 --use`).");
        }
    }
    let report = TaskJson {
        task: task.title(),
        used: install.is_some(),
        install: install.as_ref().map(|i| InstallJson::of(i, Some(Source::Runtime))),
        outcome,
    };
    print_task(ctx, &report, install.as_ref().filter(|_| cfg.runtime.is_active()))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
