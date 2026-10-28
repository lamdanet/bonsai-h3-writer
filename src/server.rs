//! Finds the Bonsai-demo install and starts/stops its llama-server.
//!
//! The user's install (`D:\Bonsai-demo`) contains:
//! - `scripts\start_llama_server.ps1` - the official launcher. It picks the model in
//!   `models\bonsai2-gguf\27B\` (first `*-PQ2_0.gguf`, else first `*-PTQ1_0.gguf`; the mmproj next
//!   to it), the first existing `bin\{cuda,hip,vulkan,cpu}\llama-server.exe`, and the right flags;
//!   it runs llama-server in the foreground on 127.0.0.1:8080 and exits 1 if something already
//!   answers `/health` on 8080. It reads env vars: `BONSAI_CTX`, `BONSAI_MMPROJ_CPU`, `BONSAI_KV4`,
//!   `BONSAI_NGL`, ...
//! - `start_bonsai.bat` - the user's own launcher (starts the .ps1 in the background).
//! - `bin\<backend>\llama-server.exe`, `models\...\*.gguf`.
//!
//! Start = spawn `powershell -NoProfile -ExecutionPolicy Bypass -File <root>\scripts\start_llama_server.ps1`
//! hidden (Windows `CREATE_NO_WINDOW`), working dir = root, extra env vars from [`StartOptions`],
//! stdout+stderr appended to [`ServerManager::log_path`] after a `=== <timestamp> start ===` line.
//! Then poll `/health` until 200.
//!
//! Without the launcher script (but with llama-server.exe and a model) the exe is started directly
//! with the launcher's essential flags: `-m <model> --host 127.0.0.1 --port <port of the server URL,
//! 8080> -ngl <BONSAI_NGL or 99 (0 for bin\cpu)> -fa on -c <BONSAI_CTX or 16384> --jinja
//! [--mmproj <mmproj> [--no-mmproj-offload when BONSAI_MMPROJ_CPU=1]]`, plus like the launcher
//! the Q4_0 KV cache for `BONSAI_KV4=1` (with a `*kv-bias*.gguf` next to the model:
//! `--kv-mean-center` and `LLAMA_ATTN_ROT_DISABLE=1`) and the image-token cap
//! (`BONSAI_IMAGE_MAX_TOKENS`, else 1024 unless the build is CUDA/HIP). Other `BONSAI_*` variables
//! are named in the log as ignored.
//!
//! Stop = kill our powershell child's process tree and the `llama-server.exe` that listens on the
//! server URL's port (`netstat -ano`, image name checked with `tasklist`), also one started by
//! start_bonsai.bat. Other llama-server processes (another port, another app) are never touched.
//! The model needs ~7.7 GB of the 8 GB GPU, so ComfyUI must not run at the same time.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::client::{BonsaiClient, Health};
use crate::config::AppConfig;
use crate::gpu;

/// Windows `CREATE_NO_WINDOW`: console programs run without a console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Windows `CREATE_BREAKAWAY_FROM_JOB`: lets the server outlive a job object the app runs in
/// (tried first; dropped when the job forbids breakaway).
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Folder name of the Bonsai demo repository.
const DEMO_DIR_NAME: &str = "Bonsai-demo";
/// `scripts\start_llama_server.ps1`, relative to the install root.
const LAUNCHER_PS1_PARTS: [&str; 2] = ["scripts", "start_llama_server.ps1"];
const LAUNCHER_BAT_NAME: &str = "start_bonsai.bat";
/// llama-server builds in the launcher's order (`bin\cuda` first, local builds last).
const SERVER_EXE_DIRS: [&[&str]; 6] = [
    &["bin", "cuda"],
    &["bin", "hip"],
    &["bin", "vulkan"],
    &["bin", "cpu"],
    &["llama.cpp", "build", "bin", "Release"],
    &["llama.cpp", "build", "bin"],
];
const SERVER_EXE_NAME: &str = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
/// Where the launcher looks for the default (Bonsai 2 27B) model.
const LAUNCHER_MODEL_DIR: [&str; 3] = ["models", "bonsai2-gguf", "27B"];
/// Model folders deeper than this under `models\` are not searched.
const MAX_MODEL_DEPTH: usize = 6;

/// The launcher's port (also used for a direct start when the server URL names no port).
const DEFAULT_PORT: u16 = 8080;
/// Context for a direct start without `BONSAI_CTX` (what the launcher picks on a 16 GB machine).
const DIRECT_DEFAULT_CTX: &str = "16384";
/// How much of the log end is read for tails and failure messages.
const LOG_TAIL_BYTES: u64 = 256 * 1024;
/// The log is renamed to `server.log.old` at the next start once it is larger than this.
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// How long [`ServerManager::stop`] waits for the server to go away.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval of [`ServerManager::wait_ready`].
const WAIT_POLL: Duration = Duration::from_secs(1);
/// Timeout of helper tools (`tasklist`, `taskkill`).
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `start` gives a `llama-server.exe` that does not answer yet (e.g. just launched by
/// start_bonsai.bat) to show up on the server URL or to go away.
const STRAY_WAIT: Duration = Duration::from_secs(3);
/// `/health` misses tolerated while Ready before the state drops (a busy server can be slow).
const READY_MISSES: u32 = 1;
/// TCP connect timeout for "does anything listen on the loopback port" (a listening socket
/// accepts in microseconds, even while the server is busy).
const LOOPBACK_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// What was found in a Bonsai-demo folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BonsaiInstall {
    pub root: PathBuf,
    /// `scripts\start_llama_server.ps1`.
    pub launcher_ps1: Option<PathBuf>,
    /// `start_bonsai.bat` (informational).
    pub launcher_bat: Option<PathBuf>,
    /// First existing `bin\{cuda,hip,vulkan,cpu}\llama-server.exe`.
    pub server_exe: Option<PathBuf>,
    /// Model GGUFs (not mmproj / dspark / kv-bias) under `models\**`.
    pub models: Vec<PathBuf>,
    /// First `*mmproj*.gguf` under `models\**`.
    pub mmproj: Option<PathBuf>,
}

impl BonsaiInstall {
    /// Inspect `dir`. Errors when it does not look like a Bonsai-demo folder
    /// (no launcher script and no llama-server.exe).
    ///
    /// Picking the `scripts` subfolder by mistake is corrected to its parent. Models are sorted by
    /// path (case-insensitive, like PowerShell's `Get-ChildItem`); `mmproj` prefers the projector
    /// next to [`primary_model`](Self::primary_model), as the launcher does. Read-only.
    pub fn detect(dir: &Path) -> anyhow::Result<Self> {
        if !dir.is_dir() {
            bail!("{} does not exist or is not a folder", dir.display());
        }
        let mut root = dir.to_path_buf();
        if !join_parts(&root, &LAUNCHER_PS1_PARTS).is_file() && root.join(LAUNCHER_PS1_PARTS[1]).is_file() {
            if let Some(parent) = root.parent() {
                root = parent.to_path_buf();
            }
        }

        let launcher_ps1 = Some(join_parts(&root, &LAUNCHER_PS1_PARTS)).filter(|p| p.is_file());
        let launcher_bat = Some(root.join(LAUNCHER_BAT_NAME)).filter(|p| p.is_file());
        let server_exe =
            SERVER_EXE_DIRS.iter().map(|parts| join_parts(&root, parts).join(SERVER_EXE_NAME)).find(|p| p.is_file());
        if launcher_ps1.is_none() && server_exe.is_none() {
            bail!(
                "{} is not a Bonsai-demo folder: neither scripts\\start_llama_server.ps1 nor \
                 bin\\<cuda|hip|vulkan|cpu>\\llama-server.exe was found",
                root.display()
            );
        }

        let mut ggufs = Vec::new();
        collect_ggufs(&root.join("models"), 0, &mut ggufs);
        ggufs.sort_by_cached_key(|p| p.to_string_lossy().to_lowercase());
        let mut models = Vec::new();
        let mut mmprojs = Vec::new();
        for path in ggufs {
            let name = file_name_lower(&path);
            if name.contains("mmproj") {
                mmprojs.push(path);
            } else if !name.contains("dspark") && !name.contains("kv-bias") {
                models.push(path);
            }
        }

        let mut install = Self { root, launcher_ps1, launcher_bat, server_exe, models, mmproj: None };
        let model_dir = install.primary_model().and_then(|m| m.parent()).map(Path::to_path_buf);
        install.mmproj = mmprojs
            .iter()
            .find(|p| match (&model_dir, p.parent()) {
                (Some(want), Some(dir)) => same_path(want, dir),
                _ => false,
            })
            .or_else(|| mmprojs.first())
            .cloned();
        Ok(install)
    }

    /// True when the app can start the server: llama-server.exe and a model are present (the
    /// launcher script is used when it exists, otherwise the exe is started directly).
    pub fn is_usable(&self) -> bool {
        self.problems().is_empty()
    }

    /// Problems that stop the server from starting (empty when usable): no llama-server.exe (and
    /// then also a missing launcher), no model.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.server_exe.is_none() {
            if self.launcher_ps1.is_none() {
                out.push(r"Launcher scripts\start_llama_server.ps1 not found".to_string());
            }
            out.push(
                r"llama-server.exe not found (bin\cuda, bin\hip, bin\vulkan or bin\cpu) - run setup.ps1 in the Bonsai folder"
                    .to_string(),
            );
        }
        if self.models.is_empty() {
            out.push(r"No model (*.gguf) found under models\ - run setup.ps1 in the Bonsai folder".to_string());
        }
        out
    }

    /// Notes that do not stop the server from starting (no launcher script, no vision projector,
    /// no model where the launcher looks by default).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.launcher_ps1.is_none() && self.server_exe.is_some() {
            out.push(
                r"scripts\start_llama_server.ps1 not found - llama-server.exe is started directly with default flags"
                    .to_string(),
            );
        }
        if self.mmproj.is_none() && !self.models.is_empty() {
            out.push("No vision projector (*mmproj*.gguf) - image input is disabled".to_string());
        }
        if self.launcher_ps1.is_some() && !self.models.is_empty() && !self.has_launcher_default_model() {
            out.push(
                r"The launcher looks for *-PQ2_0.gguf or *-PTQ1_0.gguf in models\bonsai2-gguf\27B and none is there - set BONSAI_FAMILY / BONSAI_MODEL in the launch options"
                    .to_string(),
            );
        }
        out
    }

    /// The model the launcher will pick: in `models\bonsai2-gguf\27B` the first `*-PQ2_0.gguf`,
    /// else the first `*-PTQ1_0.gguf` (the launcher's order); elsewhere the same patterns; else
    /// the first model.
    pub fn primary_model(&self) -> Option<&PathBuf> {
        let launcher_dir = join_parts(&self.root, &LAUNCHER_MODEL_DIR);
        self.models
            .iter()
            .enumerate()
            .min_by_key(|(i, p)| {
                let rank = model_rank(p);
                let in_launcher_dir = p.parent().is_some_and(|d| same_path(d, &launcher_dir));
                (!(in_launcher_dir && rank < 2), rank, *i)
            })
            .map(|(_, p)| p)
    }

    /// Backend of [`server_exe`](Self::server_exe): `CUDA`, `ROCm (HIP)`, `Vulkan`, `CPU` or
    /// `local build`.
    pub fn backend(&self) -> Option<&'static str> {
        let exe = self.server_exe.as_ref()?;
        let dir = exe.parent().map(file_name_lower).unwrap_or_default();
        Some(match dir.as_str() {
            "cuda" => "CUDA",
            "hip" => "ROCm (HIP)",
            "vulkan" => "Vulkan",
            "cpu" => "CPU",
            _ => "local build",
        })
    }

    /// Multi-line human summary (paths found / missing).
    pub fn summary(&self) -> String {
        const OK: &str = "✔";
        const BAD: &str = "❌";
        const WARN: &str = "⚠";
        let mut out = vec![format!("Bonsai folder: {}", self.root.display())];
        out.push(match &self.launcher_ps1 {
            Some(p) => format!("{OK} Launcher: {}", self.rel(p)),
            None if self.server_exe.is_some() => {
                format!(
                    r"{WARN} Launcher: scripts\start_llama_server.ps1 not found (llama-server.exe is started directly)"
                )
            }
            None => format!(r"{BAD} Launcher: scripts\start_llama_server.ps1 not found"),
        });
        out.push(match &self.launcher_bat {
            Some(p) => format!("{OK} Your launcher: {}", self.rel(p)),
            None => format!("- Your launcher: {LAUNCHER_BAT_NAME} not found (optional)"),
        });
        out.push(match &self.server_exe {
            Some(p) => format!("{OK} Server: {} ({})", self.rel(p), self.backend().unwrap_or("?")),
            None => format!(r"{BAD} Server: llama-server.exe not found (bin\cuda, bin\hip, bin\vulkan, bin\cpu)"),
        });
        match self.primary_model() {
            Some(m) => {
                let size = std::fs::metadata(m).map(|md| format!(" ({})", fmt_bytes(md.len()))).unwrap_or_default();
                out.push(format!("{OK} Model: {}{size}", self.rel(m)));
                if self.models.len() > 1 {
                    out.push(format!("  + {} more model file(s) under models\\", self.models.len() - 1));
                }
            }
            None => out.push(format!(r"{BAD} Model: no *.gguf found under models\")),
        }
        out.push(match &self.mmproj {
            Some(p) => format!("{OK} Vision projector: {}", self.rel(p)),
            None => format!("{WARN} Vision projector: no *mmproj*.gguf (image input disabled)"),
        });
        if self.launcher_ps1.is_some() && !self.models.is_empty() && !self.has_launcher_default_model() {
            out.push(format!(
                r"{WARN} The launcher looks in models\bonsai2-gguf\27B for *-PQ2_0.gguf / *-PTQ1_0.gguf - none there"
            ));
        }
        let problems = self.problems();
        if problems.is_empty() {
            out.push("Ready: Bonsai can be started from this folder.".to_string());
        } else {
            out.push(format!("Not usable: {}", problems.join("; ")));
        }
        out.join("\n")
    }

    /// `models\bonsai2-gguf\27B` holds a `*-PQ2_0.gguf` or `*-PTQ1_0.gguf` (what the launcher loads
    /// with its default env).
    fn has_launcher_default_model(&self) -> bool {
        let dir = join_parts(&self.root, &LAUNCHER_MODEL_DIR);
        self.models.iter().any(|m| model_rank(m) < 2 && m.parent().is_some_and(|d| same_path(d, &dir)))
    }

    /// `path` relative to the root when inside it.
    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root).unwrap_or(path).display().to_string()
    }
}

/// Existing folders that are likely Bonsai installs (`D:\Bonsai-demo`, `C:\Bonsai-demo`,
/// `%USERPROFILE%\Bonsai-demo`, `%USERPROFILE%\Documents\Bonsai-demo`, `..\Bonsai-demo` of the exe, ...).
///
/// Order: `D:\`, `C:\`, `E:\Bonsai-demo`, `%USERPROFILE%\{,Documents\,Desktop\}Bonsai-demo` (also
/// the redirected Documents / Desktop known folders), then the current folder and the folders
/// around the running exe when they hold `scripts\start_llama_server.ps1`. Only existing folders,
/// without duplicates (case-insensitive on Windows).
pub fn candidate_dirs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        for drive in ['D', 'C', 'E'] {
            out.push(PathBuf::from(format!("{drive}:\\{DEMO_DIR_NAME}")));
        }
    }
    let home = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(dirs::home_dir);
    if let Some(home) = &home {
        out.push(home.join(DEMO_DIR_NAME));
        out.push(home.join("Documents").join(DEMO_DIR_NAME));
        out.push(home.join("Desktop").join(DEMO_DIR_NAME));
    }
    out.extend(dirs::document_dir().map(|d| d.join(DEMO_DIR_NAME)));
    out.extend(dirs::desktop_dir().map(|d| d.join(DEMO_DIR_NAME)));

    let mut near: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        near.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        near.extend(exe.ancestors().skip(1).map(Path::to_path_buf));
    }
    for dir in near {
        if join_parts(&dir, &LAUNCHER_PS1_PARTS).is_file() {
            out.push(dir.clone());
        }
        let sibling = dir.join(DEMO_DIR_NAME);
        if join_parts(&sibling, &LAUNCHER_PS1_PARTS).is_file() {
            out.push(sibling);
        }
    }

    let mut seen = HashSet::new();
    out.into_iter().filter(|d| d.is_dir()).filter(|d| seen.insert(path_key(d))).collect()
}

/// [`candidate_dirs`] that pass [`BonsaiInstall::detect`].
pub fn find_installs() -> Vec<BonsaiInstall> {
    let mut seen = HashSet::new();
    candidate_dirs()
        .iter()
        .filter_map(|d| BonsaiInstall::detect(d).ok())
        .filter(|i| seen.insert(path_key(&i.root)))
        .collect()
}

/// Server lifecycle as the app sees it.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub enum ServerState {
    /// Nothing answers on the server URL.
    #[default]
    Stopped,
    /// We spawned the launcher; no answer yet.
    Starting,
    /// `/health` answers 503 (model loading).
    Loading,
    /// `/health` answers 200.
    Ready,
    /// Stop requested, process still alive.
    Stopping,
    /// Last start failed (message, e.g. the log's last error line).
    Failed(String),
}

impl ServerState {
    /// Short label for the UI (`Stopped`, `Starting...`, `Loading model...`, `Ready`, ...).
    pub fn label(&self) -> String {
        match self {
            ServerState::Stopped => "Stopped".into(),
            ServerState::Starting => "Starting...".into(),
            ServerState::Loading => "Loading model...".into(),
            ServerState::Ready => "Ready".into(),
            ServerState::Stopping => "Stopping...".into(),
            ServerState::Failed(e) => format!("Failed: {e}"),
        }
    }
    pub fn is_ready(&self) -> bool {
        matches!(self, ServerState::Ready)
    }
    /// Starting or Loading.
    pub fn is_busy(&self) -> bool {
        matches!(self, ServerState::Starting | ServerState::Loading | ServerState::Stopping)
    }
}

/// How to start the server.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartOptions {
    /// Extra environment variables for the launcher (`BONSAI_CTX`, ...).
    pub env: Vec<(String, String)>,
    /// Start even if other programs use a lot of VRAM.
    pub force: bool,
    /// VRAM (MB) used by other programs above which a non-forced start is refused (0 = never refuse).
    pub vram_busy_mb: u32,
}

impl StartOptions {
    pub fn from_config(cfg: &AppConfig) -> Self {
        Self { env: cfg.launcher_env(), force: false, vram_busy_mb: cfg.vram_busy_mb }
    }
}

/// Error of [`ServerManager::wait_ready`] / [`ServerManager::ensure_running`] when the cancel flag
/// was set (find it with `err.downcast_ref::<WaitCancelled>()`). The server keeps starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitCancelled;

impl std::fmt::Display for WaitCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled while waiting for Bonsai")
    }
}

impl std::error::Error for WaitCancelled {}

/// Owns the server process for one install.
pub struct ServerManager {
    install: BonsaiInstall,
    client: BonsaiClient,
    child: Option<Child>,
    started_by_us: bool,
    state: ServerState,
    last_activity: Instant,
    started_at: Option<Instant>,
    log_path: PathBuf,
    /// The child is llama-server itself (no launcher script) rather than powershell.
    direct: bool,
    /// Log size when the current run started (where its start marker is).
    run_log_offset: u64,
    /// Consecutive tolerated `/health` misses while Ready.
    misses: u32,
}

impl ServerManager {
    /// Log file: `config::config_dir()\server.log`.
    pub fn new(install: BonsaiInstall, base_url: &str) -> Self {
        Self::with_log_path(install, base_url, default_log_path())
    }

    /// Like [`new`](Self::new) with a custom log file.
    pub fn with_log_path(install: BonsaiInstall, base_url: &str, log_path: PathBuf) -> Self {
        Self {
            install,
            client: BonsaiClient::new(base_url),
            child: None,
            started_by_us: false,
            state: ServerState::Stopped,
            last_activity: Instant::now(),
            started_at: None,
            log_path,
            direct: false,
            run_log_offset: 0,
            misses: 0,
        }
    }

    pub fn install(&self) -> &BonsaiInstall {
        &self.install
    }

    pub fn client(&self) -> &BonsaiClient {
        &self.client
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn state(&self) -> &ServerState {
        &self.state
    }

    /// True when this manager spawned the running server (auto-stop / stop-on-exit only apply then).
    pub fn started_by_us(&self) -> bool {
        self.started_by_us
    }

    /// PID of our launcher (or of llama-server in a direct start) while it runs.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Re-check: our child process alive? `/health`? Updates and returns the state.
    /// A server started elsewhere (e.g. start_bonsai.bat) shows as Ready/Loading with
    /// `started_by_us() == false`.
    ///
    /// When our launcher exits while Starting/Loading and nothing answers, the state becomes
    /// `Failed` with the most telling error line of this run's log; `Failed` stays until the next
    /// start or until a server answers. One missed `/health` while Ready is tolerated.
    pub fn refresh(&mut self) -> &ServerState {
        self.refresh_inner(true)
    }

    fn refresh_inner(&mut self, tolerate_miss: bool) -> &ServerState {
        let (child, exit_code) = self.poll_child();
        let probe = match self.probe_health() {
            Health::Ready => Probe::Ready,
            Health::Loading => Probe::Loading,
            Health::Down(_) => Probe::Down,
        };
        let prev = self.state.clone();
        let misses = self.misses;
        let (next, misses) = next_state(&prev, probe, child, misses, tolerate_miss, || self.failure_message(exit_code));
        self.misses = misses;
        if child == ChildStatus::ExitedNow || matches!(next, ServerState::Stopped | ServerState::Failed(_)) {
            // Whatever answers now is not the process we spawned.
            self.started_by_us = false;
            self.started_at = None;
        }
        self.state = next;
        &self.state
    }

    /// `/health`, except that a loopback URL where nothing listens is Down at once: on Windows a
    /// refused local connect takes ~2 s (SYN retries), which would slow every check while stopped.
    fn probe_health(&self) -> Health {
        if let Some(addrs) = loopback_addrs(self.client.base_url()) {
            let listening = addrs.iter().any(|a| TcpStream::connect_timeout(a, LOOPBACK_PROBE_TIMEOUT).is_ok());
            if !listening {
                return Health::Down(format!("nothing listens on {}", self.client.base_url()));
            }
        }
        self.client.health()
    }

    /// Our child's status; forgets the handle once it exited.
    fn poll_child(&mut self) -> (ChildStatus, Option<i32>) {
        let Some(child) = self.child.as_mut() else {
            return (ChildStatus::None, None);
        };
        match child.try_wait() {
            Ok(None) => (ChildStatus::Alive, None),
            Ok(Some(status)) => {
                self.child = None;
                (ChildStatus::ExitedNow, status.code())
            }
            Err(_) => {
                self.child = None;
                (ChildStatus::ExitedNow, None)
            }
        }
    }

    /// Spawn the launcher (non-blocking). Errors when the install is not usable, when the server
    /// already runs, or (unless `opts.force`) when other programs use more than `opts.vram_busy_mb`
    /// of VRAM (message names them, e.g. "python.exe (ComfyUI?) uses 6.1 GB").
    ///
    /// The folder is re-detected first (models may have changed). A `llama-server.exe` that runs
    /// but does not answer gets a few seconds to come up or go away; after that a non-forced start
    /// is refused.
    pub fn start(&mut self, opts: &StartOptions) -> anyhow::Result<()> {
        self.ensure_nothing_answers()?;
        if let Some(child) = self.child.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                bail!("Bonsai is already starting (process {})", child.id());
            }
            self.child = None;
        }
        let root = self.install.root.clone();
        // detect's message names the folder and the reason; no extra context layer, so a plain
        // `{}` display still shows why.
        self.install = BonsaiInstall::detect(&root)?;
        let problems = self.install.problems();
        if !problems.is_empty() {
            bail!("Bonsai cannot be started from {}: {}", root.display(), problems.join("; "));
        }
        self.wait_out_stray_servers(opts.force)?;
        if !opts.force && opts.vram_busy_mb > 0 {
            if let Some(info) = gpu::query() {
                if let Some(msg) = gpu::vram_busy_message(&info, opts.vram_busy_mb) {
                    bail!("Not starting Bonsai: {msg}");
                }
            }
        }
        self.spawn_server(opts)
    }

    /// Error (and state Ready/Loading) when a server already answers on the URL.
    fn ensure_nothing_answers(&mut self) -> anyhow::Result<()> {
        let url = self.client.base_url().to_string();
        match self.probe_health() {
            Health::Ready => {
                self.state = ServerState::Ready;
                bail!("Bonsai is already running at {url}");
            }
            Health::Loading => {
                self.state = ServerState::Loading;
                bail!("Bonsai is already running at {url} (still loading the model)");
            }
            Health::Down(_) => Ok(()),
        }
    }

    /// A `llama-server.exe` on our port or from our install folder that does not answer (yet): wait
    /// up to [`STRAY_WAIT`] for it to answer (-> already running) or exit; then refuse unless
    /// `force`. Other llama-server processes on the PC do not matter.
    fn wait_out_stray_servers(&mut self, force: bool) -> anyhow::Result<()> {
        let deadline = Instant::now() + STRAY_WAIT;
        loop {
            let pids = self.server_pids();
            if pids.is_empty() {
                return Ok(());
            }
            self.ensure_nothing_answers()?;
            if Instant::now() >= deadline {
                if force {
                    return Ok(());
                }
                let list: Vec<String> = pids.iter().map(u32::to_string).collect();
                bail!(
                    "llama-server.exe is already running (PID {}) but does not answer on {}. It may still be \
                     starting (start_bonsai.bat?) or be stuck - stop it first, then start again.",
                    list.join(", "),
                    self.client.base_url()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Open the log, write the start marker and spawn the launcher (or llama-server directly).
    fn spawn_server(&mut self, opts: &StartOptions) -> anyhow::Result<()> {
        let launch = self.launch_command(opts)?;
        rotate_log(&self.log_path);
        if let Some(dir) = self.log_path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .with_context(|| format!("opening the server log {}", self.log_path.display()))?;
        let offset = log.metadata().map(|m| m.len()).unwrap_or(0);
        let mut header =
            format!("=== {} start ===\n[bh3] {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), launch.shown);
        if !opts.env.is_empty() {
            header.push_str(&format!("[bh3] env: {}\n", describe_env(&opts.env)));
        }
        for note in &launch.notes {
            header.push_str(&format!("[bh3] {note}\n"));
        }
        log.write_all(header.as_bytes())
            .and_then(|_| log.flush())
            .with_context(|| format!("writing the server log {}", self.log_path.display()))?;
        let stdout = log.try_clone().context("duplicating the log handle")?;
        let stderr = log.try_clone().context("duplicating the log handle")?;
        let mut cmd = launch.cmd;
        cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        let child = spawn_detached(&mut cmd).with_context(|| format!("starting {}", launch.shown))?;

        self.child = Some(child);
        self.direct = launch.direct;
        self.started_by_us = true;
        self.started_at = Some(Instant::now());
        self.state = ServerState::Starting;
        self.run_log_offset = offset;
        self.misses = 0;
        self.touch();
        Ok(())
    }

    /// The command that starts the server: the launcher script, else llama-server.exe directly.
    fn launch_command(&self, opts: &StartOptions) -> anyhow::Result<Launch> {
        let root = &self.install.root;
        if let Some(ps1) = &self.install.launcher_ps1 {
            let program = if cfg!(windows) { "powershell.exe" } else { "pwsh" };
            let mut cmd = Command::new(program);
            cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(ps1).current_dir(root);
            cmd.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            let shown = format!("{program} -NoProfile -ExecutionPolicy Bypass -File \"{}\"", ps1.display());
            return Ok(Launch { cmd, shown, direct: false, notes: Vec::new() });
        }

        let exe = self.install.server_exe.as_ref().context("llama-server.exe not found")?;
        let model = self.install.primary_model().context("no model (*.gguf) found under models")?;
        let build = exe.parent().map(file_name_lower).unwrap_or_default();
        let is_cpu_build = build == "cpu";
        let kv4 = env_value(opts, "BONSAI_KV4").is_some_and(|v| v.trim() == "1");
        let settings = DirectLaunch {
            model,
            mmproj: self.install.mmproj.as_deref(),
            port: port_from_url(self.client.base_url()).unwrap_or(DEFAULT_PORT),
            ctx: env_value(opts, "BONSAI_CTX")
                .map(|v| v.trim().to_string())
                .filter(|v| v.parse::<u32>().is_ok_and(|n| n > 0))
                .unwrap_or_else(|| DIRECT_DEFAULT_CTX.to_string()),
            ngl: env_value(opts, "BONSAI_NGL")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| if is_cpu_build { "0" } else { "99" }.to_string()),
            mmproj_cpu: env_value(opts, "BONSAI_MMPROJ_CPU")
                .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")),
            kv4,
            // Like the launcher: the mean-centering bias next to the model (scripts/make_kv_bias.sh).
            kv_bias: if kv4 { model.parent().and_then(find_kv_bias) } else { None },
            // Like the launcher: capped unless the CUDA/HIP build runs; 0 = no cap.
            image_max_tokens: match env_value(opts, "BONSAI_IMAGE_MAX_TOKENS").map(|v| v.trim().to_string()) {
                Some(v) if !v.is_empty() => (v != "0").then_some(v),
                _ => (!matches!(build.as_str(), "cuda" | "hip")).then(|| DIRECT_IMAGE_MAX_TOKENS.to_string()),
            },
        };
        let args = direct_args(&settings);
        let mut cmd = Command::new(exe);
        cmd.args(&args).current_dir(root);
        // Like the launcher: the binary's folder (CUDA runtime DLLs) first on PATH.
        if let Some(bin_dir) = exe.parent() {
            let old = std::env::var_os("PATH").unwrap_or_default();
            let path_dirs = std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&old));
            if let Ok(path) = std::env::join_paths(path_dirs) {
                cmd.env("PATH", path);
            }
        }
        cmd.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if settings.kv_bias.is_some() {
            // The bias is calibrated with K-rotation off; the launcher sets this too.
            cmd.env("LLAMA_ATTN_ROT_DISABLE", "1");
        }
        let shown_args: Vec<String> = args.iter().map(|a| quote_for_log(a)).collect();
        let shown = format!("\"{}\" {}", exe.display(), shown_args.join(" "));
        let ignored = ignored_direct_env(&opts.env);
        let notes = if ignored.is_empty() {
            Vec::new()
        } else {
            vec![format!("ignored without the launcher script: {}", ignored.join(", "))]
        };
        Ok(Launch { cmd, shown, direct: true, notes })
    }

    /// Message for a start that ended: the best error line of this run's log, else the exit code.
    fn failure_message(&self, exit_code: Option<i32>) -> String {
        let lines = read_log_lines(&self.log_path, self.run_log_offset, LOG_TAIL_BYTES).unwrap_or_default();
        if let Some(line) = pick_failure_line(&lines) {
            return line;
        }
        let what = if self.direct { "llama-server" } else { "The launcher" };
        let code = exit_code.map(|c| format!(" with code {c}")).unwrap_or_default();
        match fallback_failure_line(&lines) {
            Some(l) => format!("{what} exited{code}: {l}"),
            None => format!("{what} exited{code} (see the server log)"),
        }
    }

    /// Last `max_lines` lines of the current run (from its start marker).
    fn run_log_tail(&self, max_lines: usize) -> Vec<String> {
        let mut lines = read_log_lines(&self.log_path, self.run_log_offset, LOG_TAIL_BYTES).unwrap_or_default();
        let skip = lines.len().saturating_sub(max_lines);
        lines.drain(..skip);
        lines
    }

    /// Poll every ~1 s until Ready, the launcher exits (Failed, with the log's last lines), `timeout`,
    /// or `cancel`. Calls `progress` whenever the state changes.
    ///
    /// `progress` also gets the first state seen. Cancel returns [`WaitCancelled`] and leaves the
    /// server starting; a timeout leaves it running too. Errors right away when nothing runs and
    /// nothing is starting.
    pub fn wait_ready(
        &mut self,
        timeout: Duration,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ServerState),
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut reported: Option<ServerState> = None;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(WaitCancelled.into());
            }
            let state = self.refresh_inner(false).clone();
            if reported.as_ref() != Some(&state) {
                progress(&state);
                reported = Some(state.clone());
            }
            match &state {
                ServerState::Ready => {
                    self.touch();
                    return Ok(());
                }
                ServerState::Failed(msg) => {
                    bail!("Bonsai failed to start: {msg}{}", fmt_log_excerpt(&self.run_log_tail(8)));
                }
                ServerState::Stopped if self.child.is_none() && self.server_pids().is_empty() => {
                    bail!("Bonsai is not running and nothing is starting it");
                }
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "Bonsai did not become ready within {} s (state: {}){}",
                    timeout.as_secs(),
                    state.label(),
                    fmt_log_excerpt(&self.run_log_tail(8))
                );
            }
            sleep_unless_cancelled(WAIT_POLL.min(deadline - now), cancel);
        }
    }

    /// Ready already -> Ok(false). Otherwise start + wait -> Ok(true) when this manager started the
    /// server that is now ready (it is [`started_by_us`](Self::started_by_us)).
    ///
    /// A server that is already starting or loading (ours or start_bonsai.bat's), or that comes up
    /// while this start begins, is waited for and gives Ok(false).
    pub fn ensure_running(
        &mut self,
        opts: &StartOptions,
        timeout: Duration,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(&ServerState),
    ) -> anyhow::Result<bool> {
        let state = self.refresh_inner(false).clone();
        match state {
            ServerState::Ready => Ok(false),
            ServerState::Starting | ServerState::Loading | ServerState::Stopping => {
                self.wait_ready(timeout, cancel, progress)?;
                Ok(false)
            }
            ServerState::Stopped | ServerState::Failed(_) => {
                if cancel.load(Ordering::Relaxed) {
                    return Err(WaitCancelled.into());
                }
                if let Err(e) = self.start(opts) {
                    // A server answered meanwhile (start_bonsai.bat, another bh3, a missed
                    // /health): `start` set the state to it, so wait for that one instead.
                    if !matches!(self.state, ServerState::Ready | ServerState::Loading) {
                        return Err(e);
                    }
                }
                self.wait_ready(timeout, cancel, progress)?;
                // Our launcher may have lost the port to another server that is now the ready one.
                Ok(self.started_by_us)
            }
        }
    }

    /// Stop the server (also one started by start_bonsai.bat). Waits up to ~10 s for /health to stop answering.
    ///
    /// Kills our launcher's process tree, then the `llama-server.exe` that listens on the server
    /// URL's port - nothing else, so other llama-server processes (another port, another app) keep
    /// running. After `/health` stops answering it also waits (within the same 10 s) for the killed
    /// server to exit, so the VRAM is free.
    pub fn stop(&mut self) -> anyhow::Result<()> {
        self.state = ServerState::Stopping;
        let mut errors: Vec<String> = Vec::new();
        // Looked up first: once our launcher's tree is gone, its llama-server no longer listens.
        let targets = self.port_server_pids();
        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                if let Err(e) = kill_process_tree(child.id()) {
                    errors.push(e);
                }
                let _ = child.kill();
            }
            wait_child_exit(&mut child, Duration::from_secs(3));
        }
        for pid in &targets {
            if let Err(e) = kill_process_tree(*pid) {
                errors.push(e);
            }
        }
        let detail = if errors.is_empty() { String::new() } else { format!(" ({})", errors.join("; ")) };

        let deadline = Instant::now() + STOP_TIMEOUT;
        let stopped_answering = loop {
            if matches!(self.probe_health(), Health::Down(_)) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        if !stopped_answering {
            self.refresh_inner(false);
            bail!("Bonsai still answers on {} after {} s{detail}", self.client.base_url(), STOP_TIMEOUT.as_secs());
        }
        self.state = ServerState::Stopped;
        self.started_by_us = false;
        self.started_at = None;
        self.misses = 0;

        // The port closes before the process is gone; the VRAM is free only after it exits.
        let still_running = || -> Vec<u32> {
            if targets.is_empty() {
                return Vec::new();
            }
            llama_server_pids().into_iter().filter(|pid| targets.contains(pid)).collect()
        };
        let mut pids = still_running();
        while !pids.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            pids = still_running();
        }
        if !pids.is_empty() {
            let list: Vec<String> = pids.iter().map(u32::to_string).collect();
            bail!("Bonsai stopped answering, but llama-server.exe (PID {}) is still running{detail}", list.join(", "));
        }
        Ok(())
    }

    /// The `llama-server.exe` processes this manager answers for: the one listening on the server
    /// URL's port and any started from the install folder (e.g. by start_bonsai.bat, not
    /// listening yet). Other llama-server processes on the PC (another port, another app) are
    /// left out. Runs `tasklist` (and `netstat` when a llama-server runs at all).
    pub fn server_pids(&self) -> Vec<u32> {
        let all = llama_server_pids();
        let mut pids = self.port_server_pids_among(&all);
        let roots = install_roots(&self.install.root);
        for pid in all {
            let ours = || process_image_path(pid).is_some_and(|exe| roots.iter().any(|root| is_under(&exe, root)));
            if !roots.is_empty() && !pids.contains(&pid) && ours() {
                pids.push(pid);
            }
        }
        pids
    }

    /// The `llama-server.exe` processes listening on the server URL's port (what [`stop`](Self::stop)
    /// kills). Empty for a URL that is not on this machine.
    fn port_server_pids(&self) -> Vec<u32> {
        self.port_server_pids_among(&llama_server_pids())
    }

    /// Those of `llama` (llama-server PIDs) that listen on the server URL's port.
    fn port_server_pids_among(&self, llama: &[u32]) -> Vec<u32> {
        let Some(port) = loopback_addrs(self.client.base_url()).and_then(|addrs| addrs.first().map(SocketAddr::port))
        else {
            return Vec::new();
        };
        if llama.is_empty() {
            return Vec::new();
        }
        listening_pids(port).into_iter().filter(|pid| llama.contains(pid)).collect()
    }

    /// Mark activity now (resets the idle timer).
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Time since the last [`touch`](Self::touch).
    pub fn idle_for(&self) -> Duration {
        self.last_activity.elapsed()
    }

    /// Time since we started the server.
    pub fn uptime(&self) -> Option<Duration> {
        self.started_at.map(|t| t.elapsed())
    }

    /// Last `max_lines` lines of the server log (lossy UTF-8; PowerShell may write UTF-16 - handle both).
    ///
    /// Reads only the last 256 KB; ANSI escape codes are removed.
    pub fn log_tail(&self, max_lines: usize) -> Vec<String> {
        read_log_tail(&self.log_path, max_lines)
    }
}

/// `config::config_dir()\server.log`, the log [`ServerManager::new`] uses.
pub fn default_log_path() -> PathBuf {
    crate::config::config_dir().join("server.log")
}

/// Last `max_lines` lines of a server log file (see [`ServerManager::log_tail`]); empty when the
/// file is missing.
pub fn read_log_tail(path: &Path, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let mut lines = read_log_lines(path, 0, LOG_TAIL_BYTES).unwrap_or_default();
    let skip = lines.len().saturating_sub(max_lines);
    lines.drain(..skip);
    lines
}

/// PIDs of running `llama-server.exe` processes (`tasklist /FI "IMAGENAME eq llama-server.exe" /FO CSV /NH`).
pub fn llama_server_pids() -> Vec<u32> {
    #[cfg(windows)]
    {
        match run_hidden("tasklist", ["/FI", "IMAGENAME eq llama-server.exe", "/FO", "CSV", "/NH"], TOOL_TIMEOUT) {
            Ok(out) if out.success => parse_tasklist_pids(&out.stdout, "llama-server.exe"),
            _ => Vec::new(),
        }
    }
    #[cfg(not(windows))]
    {
        match run_hidden("pgrep", ["-x", "llama-server"], TOOL_TIMEOUT) {
            Ok(out) => out.stdout.lines().filter_map(|l| l.trim().parse().ok()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// PIDs from `tasklist /FO CSV /NH` output (`"llama-server.exe","22240","Console","1","1,904,548 K"`)
/// whose image name is `image` (any case). The `INFO: No tasks ...` line and garbage give nothing.
pub fn parse_tasklist_pids(text: &str, image: &str) -> Vec<u32> {
    text.lines()
        .filter_map(|line| {
            let fields = parse_csv_line(line.trim());
            if fields.len() < 2 || !fields[0].trim().eq_ignore_ascii_case(image) {
                return None;
            }
            fields[1].trim().parse().ok()
        })
        .collect()
}

/// Split one CSV line (double quotes, `""` escapes).
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

/// PIDs listening on TCP `port` on a loopback or wildcard address (`netstat -ano`; `lsof` elsewhere).
fn listening_pids(port: u16) -> Vec<u32> {
    #[cfg(windows)]
    {
        match run_hidden("netstat", ["-ano"], TOOL_TIMEOUT) {
            Ok(out) if out.success => parse_netstat_listeners(&out.stdout, port),
            _ => Vec::new(),
        }
    }
    #[cfg(not(windows))]
    {
        let filter = format!("-iTCP:{port}");
        match run_hidden("lsof", ["-nP", "-t", "-sTCP:LISTEN", filter.as_str()], TOOL_TIMEOUT) {
            Ok(out) => out.stdout.lines().filter_map(|l| l.trim().parse().ok()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// PIDs of `netstat -ano` lines that listen on `port` on a loopback or wildcard address, e.g.
/// `TCP  127.0.0.1:8080  0.0.0.0:0  LISTENING  22240` or `TCP  [::]:8080  [::]:0  LISTENING  7`.
/// Only the columns count (a listener's foreign address is `0.0.0.0:0` / `[::]:0`, the PID comes
/// last), not the state word, which Windows translates.
fn parse_netstat_listeners(text: &str, port: u16) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 || !cols[0].eq_ignore_ascii_case("TCP") || !matches!(cols[2], "0.0.0.0:0" | "[::]:0") {
            continue;
        }
        let Some((host, local_port)) = cols[1].rsplit_once(':') else { continue };
        if local_port.parse::<u16>() != Ok(port) || !accepts_loopback(host) {
            continue;
        }
        if let Some(pid) = cols.last().and_then(|p| p.parse::<u32>().ok()).filter(|pid| *pid != 0) {
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// A listening address that loopback clients reach: `127.x.x.x`, `::1`, `0.0.0.0` or `::`.
fn accepts_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.split('%').next().unwrap_or(host);
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// Full path of a running process's executable (`None` when it is gone or not accessible).
#[cfg(windows)]
fn process_image_path(pid: u32) -> Option<PathBuf> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn QueryFullProcessImageNameW(process: *mut c_void, flags: u32, name: *mut u16, size: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is checked before use and closed once; `buf` outlives the call and
    // `len` holds its size in u16 units, as QueryFullProcessImageNameW expects.
    let ok = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        ok
    };
    let len = (len as usize).min(buf.len());
    (ok != 0 && len > 0).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
}

/// Full path of a running process's executable (`None` when it is gone or not accessible).
#[cfg(not(windows))]
fn process_image_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

// ---------------------------------------------------------------------------------------------
// State machine (pure, unit tested)

/// What `/health` said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Ready,
    Loading,
    Down,
}

/// Our spawned process, as seen by one refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildStatus {
    /// We have no child (never started, or it exited earlier).
    None,
    Alive,
    /// It exited since the last check.
    ExitedNow,
}

/// Next state and miss counter for one refresh. `failure` builds the `Failed` message.
fn next_state(
    prev: &ServerState,
    probe: Probe,
    child: ChildStatus,
    misses: u32,
    tolerate_miss: bool,
    failure: impl FnOnce() -> String,
) -> (ServerState, u32) {
    match probe {
        Probe::Ready => (ServerState::Ready, 0),
        Probe::Loading => (ServerState::Loading, 0),
        Probe::Down => {
            if tolerate_miss && *prev == ServerState::Ready && child != ChildStatus::ExitedNow && misses < READY_MISSES
            {
                return (ServerState::Ready, misses + 1);
            }
            let next = match child {
                ChildStatus::Alive => match prev {
                    ServerState::Stopping => ServerState::Stopping,
                    _ => ServerState::Starting,
                },
                ChildStatus::ExitedNow => match prev {
                    ServerState::Starting | ServerState::Loading => ServerState::Failed(failure()),
                    ServerState::Failed(m) => ServerState::Failed(m.clone()),
                    _ => ServerState::Stopped,
                },
                ChildStatus::None => match prev {
                    ServerState::Failed(m) => ServerState::Failed(m.clone()),
                    _ => ServerState::Stopped,
                },
            };
            (next, 0)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Failure messages

/// The most telling error line of a failed start (public for tests): a GPU out-of-memory line
/// (with a hint), else the last error-ish line that is not a generic "exiting ..." line, else the
/// last error-ish line. llama.cpp's `<time> <level>` prefix is removed; PowerShell's error-record
/// detail lines (`+ CategoryInfo ...`, `At line:...`) and the app's own marker lines are skipped.
pub fn pick_failure_line(lines: &[String]) -> Option<String> {
    let errorish: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            let (level, msg) = split_level(line);
            let msg = msg.trim();
            (!msg.is_empty() && !is_noise(msg) && is_errorish(level, msg)).then(|| msg.to_string())
        })
        .collect();
    if let Some(oom) = errorish.iter().find(|m| mentions_oom(m)) {
        return Some(format!("{oom} - the GPU is out of memory: close other GPU programs (ComfyUI?) and try again"));
    }
    errorish.iter().rev().find(|m| !is_generic_exit(m)).or_else(|| errorish.last()).cloned()
}

/// The failure line when nothing looks like an error: the message of PowerShell's last error
/// record (`<script>.ps1 : <message>`, plus the lines it was wrapped into), else the last line
/// that is not noise.
fn fallback_failure_line(lines: &[String]) -> Option<String> {
    let lines: Vec<&str> = lines.iter().map(|l| l.trim()).collect();
    let is_detail = |l: &str| l.is_empty() || is_noise(l) || is_position_line(l);
    if let Some(start) = lines.iter().rposition(|l| l.contains(".ps1 : ")) {
        let (_, first) = lines[start].split_once(".ps1 : ").unwrap_or_default();
        let mut message = first.trim().to_string();
        for more in lines[start + 1..].iter().take_while(|l| !is_detail(l)) {
            message.push(' ');
            message.push_str(more);
        }
        if !message.is_empty() {
            return Some(message);
        }
    }
    lines.iter().rev().find(|l| !is_detail(l)).map(|l| l.to_string())
}

/// The wrapped `char:5` end of PowerShell's `At <script>.ps1:17 char:5` line.
fn is_position_line(msg: &str) -> bool {
    msg.strip_prefix("char:").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// `119.08.628.778 E main: ...` -> (`Some('E')`, `main: ...`); other lines -> (`None`, line).
fn split_level(line: &str) -> (Option<char>, &str) {
    let trimmed = line.trim_start();
    let mut parts = trimmed.splitn(3, ' ');
    if let (Some(time), Some(level), Some(rest)) = (parts.next(), parts.next(), parts.next()) {
        let is_time = time.contains('.') && time.chars().all(|c| c.is_ascii_digit() || c == '.');
        let mut level_chars = level.chars();
        if let (true, Some(l), None) = (is_time, level_chars.next(), level_chars.next()) {
            if l.is_ascii_uppercase() {
                return (Some(l), rest);
            }
        }
    }
    (None, trimmed)
}

/// Lines that never explain a failure.
fn is_noise(msg: &str) -> bool {
    is_own_line(msg)
        || msg.starts_with('+')
        || msg.starts_with('~')
        || msg.starts_with("At line:")
        || (msg.starts_with("At ") && msg.contains(".ps1:"))
        || msg.contains("CategoryInfo")
        || msg.contains("FullyQualifiedErrorId")
}

/// The app's own marker / info lines and the launcher's `=== ... ===` banner.
fn is_own_line(msg: &str) -> bool {
    msg.starts_with("[bh3]") || msg.starts_with("===")
}

const ERROR_WORDS: [&str; 20] = [
    "[err]",
    "error",
    "failed",
    "failure",
    "fatal",
    "exception",
    "out of memory",
    "cannot ",
    "can't ",
    "couldn't",
    "could not",
    "unable to",
    "not found",
    "no such file",
    "denied",
    "abort",
    "invalid",
    "not recognized",
    "terminate called",
    "panic",
];

/// llama.cpp's own levels decide: `E` always, `I`/`D`/`T`/`W` only for out-of-memory (a warning
/// such as `common_fit_params: failed to fit params ...` is printed on every start). Lines without
/// a level count when they contain an error word.
fn is_errorish(level: Option<char>, msg: &str) -> bool {
    match level {
        Some('E') => true,
        Some('I' | 'D' | 'T' | 'W') => mentions_oom(msg),
        _ => {
            let lower = msg.to_lowercase();
            ERROR_WORDS.iter().any(|w| lower.contains(w))
        }
    }
}

fn mentions_oom(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("out of memory") || lower.contains("cudamalloc failed") || lower.contains("outofdevicememory")
}

fn is_generic_exit(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("exiting due to") || lower.contains("cleaning up before exit") || lower.starts_with("main: exiting")
}

fn fmt_log_excerpt(lines: &[String]) -> String {
    let lines: Vec<&str> = lines.iter().map(|l| l.as_str()).filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n\nLast lines of the server log:\n{}", lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------------------------
// Log reading

/// Read the log from byte `from` (or only its last `max_bytes`, whichever starts later), decode it
/// and split it into clean lines. A line cut by the byte limit is dropped.
fn read_log_lines(path: &Path, from: u64, max_bytes: u64) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let from = from.min(len);
    let start = from.max(len.saturating_sub(max_bytes));
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.take(len - start).read_to_end(&mut buf)?;
    let mut lines = split_log_lines(&decode_log_bytes(&buf));
    if start > from && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines)
}

/// Decode log bytes: UTF-8 (BOM stripped, NUL bytes dropped, invalid bytes replaced) or UTF-16LE
/// (BOM, or detected by NUL bytes in every second position, either alignment). Stray BOMs from
/// appended runs are removed.
pub fn decode_log_bytes(bytes: &[u8]) -> String {
    let text = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        utf8_lossy_without_nul(rest)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16le_lossy(rest)
    } else if let Some(offset) = utf16le_alignment(bytes) {
        utf16le_lossy(&bytes[offset..])
    } else {
        utf8_lossy_without_nul(bytes)
    };
    if text.contains('\u{feff}') {
        text.replace('\u{feff}', "")
    } else {
        text
    }
}

/// `Some(0|1)` when `bytes` look like UTF-16LE text (mostly ASCII, so a NUL in every second byte).
fn utf16le_alignment(bytes: &[u8]) -> Option<usize> {
    let pairs = bytes.len() / 2;
    if pairs == 0 {
        return None;
    }
    let (mut even, mut odd) = (0usize, 0usize);
    for (i, b) in bytes.iter().enumerate() {
        if *b == 0 {
            if i % 2 == 0 {
                even += 1;
            } else {
                odd += 1;
            }
        }
    }
    if odd * 10 >= pairs * 3 && odd > even * 4 {
        Some(0)
    } else if even * 10 >= pairs * 3 && even > odd * 4 {
        Some(1)
    } else {
        None
    }
}

fn utf16le_lossy(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16_lossy(&units).replace('\0', "")
}

fn utf8_lossy_without_nul(bytes: &[u8]) -> String {
    if bytes.contains(&0) {
        let filtered: Vec<u8> = bytes.iter().copied().filter(|b| *b != 0).collect();
        String::from_utf8_lossy(&filtered).into_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Split decoded log text into lines: CRLF/LF endings, a `\r` progress update keeps what a
/// terminal would show (the text after the last `\r`), ANSI codes and control characters removed,
/// trailing whitespace and trailing empty lines dropped.
pub fn split_log_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|raw| {
            let line = raw.trim_end_matches('\r');
            let visible = line.rsplit('\r').next().unwrap_or(line);
            strip_ansi(visible).trim_end().to_string()
        })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Remove ANSI escape sequences (CSI `ESC [ ... final`, OSC `ESC ] ... BEL|ESC \`, two-character
/// `ESC x`) and other control characters except tabs.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some('(' | ')' | '*' | '+') => {
                    chars.next();
                    chars.next();
                }
                Some(n) if ('0'..='~').contains(&n) => {
                    chars.next();
                }
                _ => {}
            },
            '\u{9b}' => {
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Rename a big log to `server.log.old` before a new run.
fn rotate_log(path: &Path) {
    let big = std::fs::metadata(path).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false);
    if big {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let old = PathBuf::from(old);
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(path, &old);
    }
}

// ---------------------------------------------------------------------------------------------
// Direct start (no launcher script)

/// A spawnable start command plus how it is shown in the log.
struct Launch {
    cmd: Command,
    shown: String,
    /// llama-server itself, not the launcher.
    direct: bool,
    /// Extra `[bh3]` lines for the log header.
    notes: Vec<String>,
}

/// Flags for starting llama-server without the launcher.
struct DirectLaunch<'a> {
    model: &'a Path,
    mmproj: Option<&'a Path>,
    port: u16,
    ctx: String,
    ngl: String,
    mmproj_cpu: bool,
    /// `BONSAI_KV4=1`: Q4_0 KV cache.
    kv4: bool,
    /// `*kv-bias*.gguf` next to the model (used with `kv4`).
    kv_bias: Option<PathBuf>,
    /// `--image-max-tokens` (with a vision projector only).
    image_max_tokens: Option<String>,
}

/// Image-token cap of a direct start on non-CUDA/HIP builds (the launcher's default).
const DIRECT_IMAGE_MAX_TOKENS: &str = "1024";
/// `BONSAI_*` variables a direct start understands (the launcher reads more).
const DIRECT_ENV: [&str; 5] =
    ["BONSAI_CTX", "BONSAI_NGL", "BONSAI_MMPROJ_CPU", "BONSAI_KV4", "BONSAI_IMAGE_MAX_TOKENS"];

/// `-m <model> --host 127.0.0.1 --port <port> -ngl <ngl> -fa on -c <ctx> --jinja [--mmproj <mmproj>
/// [--no-mmproj-offload]] [--cache-type-k q4_0 --cache-type-v q4_0 [--kv-mean-center <bias>]]
/// [--image-max-tokens <n>]`, like the launcher.
fn direct_args(d: &DirectLaunch<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-m".into(),
        d.model.as_os_str().to_owned(),
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        d.port.to_string().into(),
        "-ngl".into(),
        d.ngl.clone().into(),
        "-fa".into(),
        "on".into(),
        "-c".into(),
        d.ctx.clone().into(),
        "--jinja".into(),
    ];
    if let Some(mmproj) = d.mmproj {
        args.push("--mmproj".into());
        args.push(mmproj.as_os_str().to_owned());
        if d.mmproj_cpu {
            args.push("--no-mmproj-offload".into());
        }
    }
    if d.kv4 {
        args.extend(["--cache-type-k", "q4_0", "--cache-type-v", "q4_0"].map(OsString::from));
        if let Some(bias) = &d.kv_bias {
            args.push("--kv-mean-center".into());
            args.push(bias.as_os_str().to_owned());
        }
    }
    if let (Some(tokens), Some(_)) = (&d.image_max_tokens, d.mmproj) {
        args.push("--image-max-tokens".into());
        args.push(tokens.into());
    }
    args
}

/// First `*kv-bias*.gguf` in `dir` (by name), the launcher's KV4 mean-centering bias.
fn find_kv_bias(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && file_name_lower(path).contains("kv-bias") && file_name_lower(path).ends_with(".gguf")
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// `BONSAI_*` keys of `env` (uppercase, once each) that only the launcher script reads.
fn ignored_direct_env(env: &[(String, String)]) -> Vec<String> {
    let mut ignored: Vec<String> = Vec::new();
    for (key, _) in env {
        let key = key.trim().to_ascii_uppercase();
        if key.starts_with("BONSAI_") && !DIRECT_ENV.contains(&key.as_str()) && !ignored.contains(&key) {
            ignored.push(key);
        }
    }
    ignored
}

/// Host (without IPv6 brackets) and explicit port of a URL.
fn url_host_port(url: &str) -> (String, Option<u16>) {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']').unwrap_or((v6, ""));
        let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
        return (host.to_string(), port);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().ok()),
        None => (authority.to_string(), None),
    }
}

/// Explicit port of a URL (`http://127.0.0.1:8080/` -> 8080); `None` without one.
fn port_from_url(url: &str) -> Option<u16> {
    url_host_port(url).1
}

/// Socket addresses to probe when the URL points at this machine (`127.0.0.1`, `::1`,
/// `localhost` -> both); `None` for other hosts.
fn loopback_addrs(url: &str) -> Option<Vec<SocketAddr>> {
    let (host, port) = url_host_port(url);
    let default_port = if url.trim().to_ascii_lowercase().starts_with("https://") { 443 } else { 80 };
    let port = port.unwrap_or(default_port);
    if host.eq_ignore_ascii_case("localhost") {
        return Some(vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
        ]);
    }
    let ip: IpAddr = host.parse().ok()?;
    ip.is_loopback().then(|| vec![SocketAddr::new(ip, port)])
}

/// Last value of `key` in `opts.env` (any case), else the process environment.
fn env_value(opts: &StartOptions, key: &str) -> Option<String> {
    opts.env
        .iter()
        .rev()
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var(key).ok())
}

/// `BONSAI_CTX=16384, HF_TOKEN=***`: values of `BONSAI_*` / `LLAMA_*` shown, others hidden.
fn describe_env(env: &[(String, String)]) -> String {
    env.iter()
        .map(|(k, v)| {
            let upper = k.to_ascii_uppercase();
            if upper.starts_with("BONSAI_") || upper.starts_with("LLAMA_") {
                format!("{k}={v}")
            } else {
                format!("{k}=***")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_for_log(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if s.contains(' ') || s.is_empty() {
        format!("\"{s}\"")
    } else {
        s.into_owned()
    }
}

// ---------------------------------------------------------------------------------------------
// Processes

/// Output of a helper command run by [`run_hidden`].
#[derive(Debug, Clone, Default)]
pub(crate) struct CmdOutput {
    /// Exited with code 0 before the timeout.
    pub success: bool,
    /// Exit code (`None` when killed after the timeout).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run a console tool without a window, capture its output, kill it after `timeout`.
pub(crate) fn run_hidden<I, S>(program: impl AsRef<OsStr>, args: I, timeout: Duration) -> std::io::Result<CmdOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_console(&mut cmd);
    let mut child = cmd.spawn()?;
    let out_rx = drain_pipe(child.stdout.take());
    let err_rx = drain_pipe(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let grace = Duration::from_millis(500);
    let text = |rx: mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(grace).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    };
    Ok(CmdOutput {
        success: status.is_some_and(|s| s.success()),
        code: status.and_then(|s| s.code()),
        stdout: text(out_rx),
        stderr: text(err_rx),
    })
}

/// Read a child pipe to the end on a helper thread.
fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

/// No console window for the child (Windows `CREATE_NO_WINDOW`).
fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// Spawn hidden; on Windows also outside the app's job object when allowed, so the server keeps
/// running when the CLI that started it exits.
fn spawn_detached(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = cmd.spawn() {
            return Ok(child);
        }
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

/// Wait up to `timeout` for a child to exit (reaps it).
fn wait_child_exit(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            _ => return,
        }
    }
}

/// Kill a process and its children (`taskkill /F /T /PID <pid>`).
fn kill_process_tree(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        let pid = pid.to_string();
        let out = run_hidden("taskkill", ["/F", "/T", "/PID", pid.as_str()], TOOL_TIMEOUT)
            .map_err(|e| format!("taskkill: {e}"))?;
        // 128: no such process (already gone).
        if out.success || out.code == Some(128) {
            Ok(())
        } else {
            Err(tool_error("taskkill", &out))
        }
    }
    #[cfg(not(windows))]
    {
        let pid = pid.to_string();
        let out = run_hidden("kill", ["-9", pid.as_str()], TOOL_TIMEOUT).map_err(|e| format!("kill: {e}"))?;
        if out.success {
            Ok(())
        } else {
            Err(tool_error("kill", &out))
        }
    }
}

fn tool_error(tool: &str, out: &CmdOutput) -> String {
    let text = format!("{} {}", out.stderr.trim(), out.stdout.trim());
    let text = text.trim();
    match (text.is_empty(), out.code) {
        (false, _) => format!("{tool}: {text}"),
        (true, Some(code)) => format!("{tool} exited with code {code}"),
        (true, None) => format!("{tool} timed out"),
    }
}

/// Sleep up to `total`, waking early when `cancel` is set.
fn sleep_unless_cancelled(total: Duration, cancel: &AtomicBool) {
    let end = Instant::now() + total;
    while !cancel.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= end {
            break;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

// ---------------------------------------------------------------------------------------------
// Paths

fn join_parts(root: &Path, parts: &[&str]) -> PathBuf {
    parts.iter().fold(root.to_path_buf(), |p, s| p.join(s))
}

/// Collect `*.gguf` files under `dir` (follows links, skips `.hidden` folders, bounded depth).
fn collect_ggufs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if meta.is_dir() {
            if depth < MAX_MODEL_DEPTH && !name.starts_with('.') {
                collect_ggufs(&path, depth + 1, out);
            }
        } else if meta.is_file() && name.ends_with(".gguf") {
            out.push(path);
        }
    }
}

/// 0 = `*-PQ2_0.gguf`, 1 = `*-PTQ1_0.gguf`, 2 = anything else (the launcher's preference).
fn model_rank(path: &Path) -> u8 {
    let name = file_name_lower(path);
    if name.ends_with("-pq2_0.gguf") {
        0
    } else if name.ends_with("-ptq1_0.gguf") {
        1
    } else {
        2
    }
}

fn file_name_lower(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Comparison key: separators unified, no trailing separator, lowercase on Windows.
fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    let s = s.trim_end_matches('\\');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// The install folder as configured and as resolved on disk (8.3 names, links); none for an
/// empty root (no install chosen).
fn install_roots(root: &Path) -> Vec<PathBuf> {
    if root.as_os_str().is_empty() {
        return Vec::new();
    }
    let mut roots = vec![root.to_path_buf()];
    roots.extend(std::fs::canonicalize(root).ok());
    roots
}

/// True when `path` lies inside the folder `root` (any case on Windows, `\\?\` prefixes ignored).
fn is_under(path: &Path, root: &Path) -> bool {
    let (path, root) = (path_key(path), path_key(root));
    let (path, root) = (path.trim_start_matches(r"\\?\"), root.trim_start_matches(r"\\?\"));
    !root.is_empty() && path.len() > root.len() && path.starts_with(root) && path[root.len()..].starts_with('\\')
}

/// `5.4 GB` / `630 MB`.
fn fmt_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = bytes as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp folder for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bh3-server-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch_file(root: &Path, rel: &str) -> PathBuf {
        let path = rel.split(['/', '\\']).fold(root.to_path_buf(), |p, s| p.join(s));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();
        path
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tasklist_csv() {
        let text = "\"llama-server.exe\",\"22240\",\"Console\",\"2\",\"1,904,548 K\"\r\n\
                    \"LLAMA-SERVER.EXE\",\"31\",\"Console\",\"2\",\"10 K\"\r\n\
                    \"python.exe\",\"77\",\"Console\",\"2\",\"10 K\"\r\n\
                    garbage line\r\n";
        assert_eq!(parse_tasklist_pids(text, "llama-server.exe"), vec![22240, 31]);
        let none = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(parse_tasklist_pids(none, "llama-server.exe").is_empty());
        assert!(parse_tasklist_pids("", "llama-server.exe").is_empty());
        assert_eq!(parse_csv_line(r#""a ""b""",c,"d,e""#), vec!["a \"b\"", "c", "d,e"]);
    }

    #[test]
    fn decodes_utf8_with_and_without_bom() {
        assert_eq!(decode_log_bytes("héllo\nwörld".as_bytes()), "héllo\nwörld");
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice("ready ✔\n".as_bytes());
        assert_eq!(decode_log_bytes(&bom), "ready ✔\n");
        // Invalid bytes are replaced, NULs dropped, stray BOMs removed.
        assert_eq!(decode_log_bytes(b"a\xffb\0c\xEF\xBB\xBFd"), "a\u{fffd}bcd");
    }

    #[test]
    fn decodes_utf16le_with_bom_and_by_detection() {
        let text = "[ERR] No model file found\r\nZürich\r\n";
        let units: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.extend_from_slice(&units);
        assert_eq!(decode_log_bytes(&with_bom), text);
        // No BOM (a tail read from the middle), aligned and misaligned by one byte.
        assert_eq!(decode_log_bytes(&units), text);
        let misaligned = &units[1..];
        let decoded = decode_log_bytes(misaligned);
        assert!(decoded.ends_with("No model file found\r\nZürich\r\n"), "{decoded:?}");
    }

    #[test]
    fn strips_ansi_codes() {
        assert_eq!(strip_ansi("\u{1b}[1;31mERROR\u{1b}[0m done"), "ERROR done");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}text"), "text");
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{1b}\\link"), "link");
        assert_eq!(strip_ansi("a\u{1b}[2Kb\u{1b}(Bc\u{1b}=d"), "abcd");
        assert_eq!(strip_ansi("tab\tbell\u{7}"), "tab\tbell");
    }

    #[test]
    fn splits_lines() {
        let text = "one\r\ntwo\n\u{1b}[32mthree\u{1b}[0m  \r\nload 10%\rload 100%\r\n\n\n";
        assert_eq!(split_log_lines(text), lines(&["one", "two", "three", "load 100%"]));
        assert!(split_log_lines("").is_empty());
        assert_eq!(split_log_lines("a\n\nb"), lines(&["a", "", "b"]));
    }

    #[test]
    fn reads_log_tail_and_run_offset() {
        let dir = temp_dir("log");
        let path = dir.join("server.log");
        assert!(read_log_tail(&path, 10).is_empty(), "missing file -> empty");
        let mut text = String::new();
        for i in 0..100 {
            text.push_str(&format!("line {i}\r\n"));
        }
        std::fs::write(&path, &text).unwrap();
        assert_eq!(read_log_tail(&path, 3), lines(&["line 97", "line 98", "line 99"]));
        assert_eq!(read_log_tail(&path, 0), Vec::<String>::new());
        assert_eq!(read_log_tail(&path, 1000).len(), 100);
        // Byte limit: the cut first line is dropped.
        let tail = read_log_lines(&path, 0, 20).unwrap();
        assert_eq!(tail, lines(&["line 98", "line 99"]));
        // From an offset (the start of this run).
        let offset = text.find("line 95").unwrap() as u64;
        assert_eq!(read_log_lines(&path, offset, LOG_TAIL_BYTES).unwrap().len(), 5);
        // UTF-16LE file with BOM.
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("a\r\nb\r\n\u{1b}[31mc\u{1b}[0m\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        std::fs::write(&path, utf16).unwrap();
        assert_eq!(read_log_tail(&path, 2), lines(&["b", "c"]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failure_line_from_launcher_error() {
        let log = lines(&[
            "=== 2026-10-28 10:41:00 start ===",
            "[bh3] powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"D:\\Bonsai-demo\\scripts\\start_llama_server.ps1\"",
            "[ERR] No model file found for Bonsai-2-27B in D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B",
            "      Run .\\setup.ps1 first.",
        ]);
        assert_eq!(
            pick_failure_line(&log).as_deref(),
            Some("[ERR] No model file found for Bonsai-2-27B in D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B")
        );
    }

    #[test]
    fn failure_line_prefers_out_of_memory() {
        let log = lines(&[
            "0.00.967.509 I cmn  common_param: loading",
            "1.02.100.000 E ggml_backend_cuda_buffer_type_alloc_buffer: allocating 5678.00 MiB on device 0: cudaMalloc failed: out of memory",
            "1.02.100.100 E llama_model_load: error loading model: unable to allocate CUDA0 buffer",
            "1.02.200.000 E main: exiting due to model loading error",
        ]);
        let line = pick_failure_line(&log).unwrap_or_default();
        assert!(line.starts_with("ggml_backend_cuda_buffer_type_alloc_buffer: allocating 5678.00 MiB"), "{line}");
        assert!(line.contains("ComfyUI"), "{line}");
    }

    #[test]
    fn failure_line_skips_generic_exit_and_powershell_noise() {
        let log = lines(&[
            "0.01.000.000 I srv  load_model: loading model",
            "0.02.000.000 E start: couldn't bind HTTP server socket, hostname: 127.0.0.1, port: 8080",
            "0.02.000.100 E srv  main: exiting due to HTTP server error",
        ]);
        assert_eq!(
            pick_failure_line(&log).as_deref(),
            Some("start: couldn't bind HTTP server socket, hostname: 127.0.0.1, port: 8080")
        );
        let ps = lines(&[
            "D:\\Bonsai-demo\\scripts\\start_llama_server.ps1 : 9B requires BONSAI_FAMILY=bonsai2",
            "At line:1 char:1",
            "+ CategoryInfo          : NotSpecified: (:) [Write-Error], WriteErrorException",
            "    + FullyQualifiedErrorId : Microsoft.PowerShell.Commands.WriteErrorException,start_llama_server.ps1",
        ]);
        assert_eq!(pick_failure_line(&ps), None, "no error words in the message itself");
        let ps2 = lines(&[
            "Get-CimInstance : Access denied",
            "At D:\\Bonsai-demo\\scripts\\start_llama_server.ps1:124 char:5",
            "    + CategoryInfo          : PermissionDenied: (root\\cimv2:Win32_ComputerSystem:String) [Get-CimInstance], CimException",
        ]);
        assert_eq!(pick_failure_line(&ps2).as_deref(), Some("Get-CimInstance : Access denied"));
        // Info lines with error-like words do not count; nothing error-ish -> None.
        let info = lines(&["0.00.100.000 I log: --log-errors enabled", "  Press Ctrl+C to stop.", ""]);
        assert_eq!(pick_failure_line(&info), None);
        assert_eq!(split_level("119.08.628.778 W srv  stop: cancel task"), (Some('W'), "srv  stop: cancel task"));
        assert_eq!(split_level("[WARN] x y"), (None, "[WARN] x y"));
    }

    #[test]
    fn failure_fallback_skips_warnings_and_powershell_details() {
        // The warning this build prints on every start is not a failure reason (unless OOM).
        let killed = lines(&[
            "0.02.137.466 W common_fit_params: failed to fit params to free device memory: n_gpu_layers already set by user to 99, abort",
            "0.02.200.000 I llama_model_loader: loaded meta data",
        ]);
        assert_eq!(pick_failure_line(&killed), None);
        let oom = lines(&["1.00.000.000 W ggml_cuda: cudaMalloc failed: out of memory"]);
        assert!(pick_failure_line(&oom).is_some_and(|l| l.contains("ComfyUI")));
        // PowerShell's error record: its (wrapped) message, not the detail lines.
        let ps = lines(&[
            "[bh3] powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"D:\\Bonsai-demo\\scripts\\start_llama_server.ps1\"",
            "D:\\Bonsai-demo\\scripts\\start_llama_server.ps1 : 9B requires BONSAI_FAMILY=bonsai2 - set it in",
            "the extra environment",
            "At D:\\Bonsai-demo\\scripts\\start_llama_server.ps1:17",
            "char:5",
            "+     Write-Error \"${BonsaiModel} requires BONSAI_FAMILY=bonsai2\"",
            "    + CategoryInfo          : NotSpecified: (:) [Write-Error], WriteErrorException",
            "    + FullyQualifiedErrorId : Microsoft.PowerShell.Commands.WriteErrorException,start_llama_server.ps1",
        ]);
        assert_eq!(pick_failure_line(&ps), None);
        assert_eq!(
            fallback_failure_line(&ps).as_deref(),
            Some("9B requires BONSAI_FAMILY=bonsai2 - set it in the extra environment")
        );
        // Without a record: the last line that is not noise.
        let plain = lines(&["fake launcher", "char:12", "+ x", "[bh3] env: BONSAI_CTX=1", ""]);
        assert_eq!(fallback_failure_line(&plain).as_deref(), Some("fake launcher"));
        assert_eq!(fallback_failure_line(&lines(&["=== 2026 start ===", "  "])), None);
        assert!(is_position_line("char:5") && !is_position_line("char:") && !is_position_line("chart: 5"));
    }

    #[test]
    fn netstat_listeners() {
        let text = "\r\nActive Connections\r\n\r\n  Proto  Local Address          Foreign Address        State           PID\r\n\
                    \x20 TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1692\r\n\
                    \x20 TCP    127.0.0.1:8080         0.0.0.0:0              LISTENING       22240\r\n\
                    \x20 TCP    127.0.0.1:8080         127.0.0.1:57087        TIME_WAIT       0\r\n\
                    \x20 TCP    127.0.0.1:57090        127.0.0.1:8080         ESTABLISHED     4242\r\n\
                    \x20 TCP    192.168.1.5:8080       0.0.0.0:0              LISTENING       555\r\n\
                    \x20 TCP    0.0.0.0:18080          0.0.0.0:0              ABH\u{d6}REN        666\r\n\
                    \x20 TCP    [::]:8080              [::]:0                 ABH\u{d6}REN        777\r\n\
                    \x20 TCP    [::1]:8080             [::]:0                 LISTENING       22240\r\n\
                    \x20 TCP    [fe80::1%5]:8080       [::]:0                 LISTENING       888\r\n\
                    \x20 UDP    0.0.0.0:8080           *:*                                    999\r\n\
                    garbage\r\n";
        // Loopback and wildcard listeners only, each PID once; the state word is not read.
        assert_eq!(parse_netstat_listeners(text, 8080), vec![22240, 777]);
        assert_eq!(parse_netstat_listeners(text, 18080), vec![666]);
        assert!(parse_netstat_listeners(text, 57087).is_empty());
        assert!(parse_netstat_listeners("", 8080).is_empty());
        assert!(accepts_loopback("127.0.0.2") && accepts_loopback("[::]") && !accepts_loopback("10.0.0.1"));
    }

    #[test]
    fn install_folder_membership() {
        let root = Path::new(r"D:\Bonsai-demo");
        assert!(is_under(Path::new(r"D:\Bonsai-demo\bin\cuda\llama-server.exe"), root) || !cfg!(windows));
        if cfg!(windows) {
            assert!(is_under(
                Path::new(r"\\?\d:\bonsai-demo\bin\cuda\llama-server.exe"),
                Path::new(r"D:\Bonsai-demo\")
            ));
        }
        assert!(!is_under(Path::new(r"D:\Bonsai-demo-old\bin\llama-server.exe"), root));
        assert!(!is_under(root, root));
        assert!(!is_under(Path::new(r"D:\Bonsai-demo\x.exe"), Path::new("")));
        assert!(install_roots(Path::new("")).is_empty());
        assert_eq!(install_roots(Path::new(r"Z:\no\such\folder")), vec![PathBuf::from(r"Z:\no\such\folder")]);
    }

    #[test]
    fn state_machine() {
        use ChildStatus as C;
        use Probe as P;
        use ServerState as S;
        let fail = || "boom".to_string();
        assert_eq!(next_state(&S::Stopped, P::Ready, C::None, 0, true, fail), (S::Ready, 0));
        assert_eq!(next_state(&S::Starting, P::Loading, C::Alive, 0, true, fail), (S::Loading, 0));
        assert_eq!(next_state(&S::Stopped, P::Down, C::None, 0, true, fail), (S::Stopped, 0));
        assert_eq!(next_state(&S::Starting, P::Down, C::Alive, 0, true, fail), (S::Starting, 0));
        assert_eq!(next_state(&S::Starting, P::Down, C::ExitedNow, 0, true, fail), (S::Failed("boom".into()), 0));
        assert_eq!(next_state(&S::Loading, P::Down, C::ExitedNow, 0, true, fail), (S::Failed("boom".into()), 0));
        assert_eq!(next_state(&S::Ready, P::Down, C::ExitedNow, 0, true, fail), (S::Stopped, 0));
        // The launcher exited because a server already answered: Ready (not ours).
        assert_eq!(next_state(&S::Starting, P::Ready, C::ExitedNow, 0, true, fail), (S::Ready, 0));
        // One tolerated miss while Ready, then down.
        assert_eq!(next_state(&S::Ready, P::Down, C::None, 0, true, fail), (S::Ready, 1));
        assert_eq!(next_state(&S::Ready, P::Down, C::None, 1, true, fail), (S::Stopped, 0));
        assert_eq!(next_state(&S::Ready, P::Down, C::None, 0, false, fail), (S::Stopped, 0));
        assert_eq!(next_state(&S::Ready, P::Down, C::Alive, 1, true, fail), (S::Starting, 0));
        // Failed sticks until something answers.
        assert_eq!(next_state(&S::Failed("x".into()), P::Down, C::None, 0, true, fail), (S::Failed("x".into()), 0));
        assert_eq!(next_state(&S::Failed("x".into()), P::Ready, C::None, 0, true, fail), (S::Ready, 0));
        assert_eq!(next_state(&S::Stopping, P::Down, C::Alive, 0, true, fail), (S::Stopping, 0));
    }

    #[test]
    fn direct_start_flags() {
        let model = PathBuf::from(r"D:\Bonsai-demo\models\bonsai2-gguf\27B\M-PTQ1_0.gguf");
        let mmproj = PathBuf::from(r"D:\Bonsai-demo\models\bonsai2-gguf\27B\M-mmproj-Q8_0.gguf");
        let d = DirectLaunch {
            model: &model,
            mmproj: Some(&mmproj),
            port: 8080,
            ctx: "16384".into(),
            ngl: "99".into(),
            mmproj_cpu: true,
            kv4: false,
            kv_bias: None,
            image_max_tokens: None,
        };
        let args: Vec<String> = direct_args(&d).iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            vec![
                "-m",
                model.to_str().unwrap(),
                "--host",
                "127.0.0.1",
                "--port",
                "8080",
                "-ngl",
                "99",
                "-fa",
                "on",
                "-c",
                "16384",
                "--jinja",
                "--mmproj",
                mmproj.to_str().unwrap(),
                "--no-mmproj-offload",
            ]
        );
        // KV4 with its bias and the image cap, like the launcher; no cap without a projector.
        let bias = PathBuf::from(r"D:\m\X-kv-bias.gguf");
        let kv = DirectLaunch {
            mmproj_cpu: false,
            kv4: true,
            kv_bias: Some(bias.clone()),
            image_max_tokens: Some("1024".into()),
            ..d
        };
        let tail: Vec<String> = direct_args(&kv)[13..].iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let expected = ["--cache-type-k", "q4_0", "--cache-type-v", "q4_0", "--kv-mean-center", bias.to_str().unwrap()];
        assert_eq!(tail[..2], ["--mmproj".to_string(), mmproj.to_str().unwrap().to_string()]);
        assert_eq!(tail[2..8], expected.map(String::from));
        assert_eq!(tail[8..], ["--image-max-tokens".to_string(), "1024".to_string()]);
        let no_vision = DirectLaunch { mmproj: None, kv4: false, ..kv };
        assert_eq!(direct_args(&no_vision).len(), 13);

        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let ignored = ignored_direct_env(&env(&[
            ("BONSAI_CTX", "8192"),
            ("bonsai_family", "ternary"),
            ("BONSAI_KV4", "1"),
            ("BONSAI_FAMILY", "x"),
            ("HF_TOKEN", "t"),
        ]));
        assert_eq!(ignored, vec!["BONSAI_FAMILY".to_string()]);
    }

    /// The launcher's KV4 bias lookup and the direct start's env handling on a fake install.
    #[test]
    fn direct_start_reads_kv4_bias_and_image_cap() {
        let root = temp_dir("direct-kv");
        let exe = touch_file(&root, &format!("bin/vulkan/{SERVER_EXE_NAME}"));
        let model = touch_file(&root, "models/bonsai2-gguf/27B/M-PTQ1_0.gguf");
        touch_file(&root, "models/bonsai2-gguf/27B/M-mmproj-Q8_0.gguf");
        let bias = touch_file(&root, "models/bonsai2-gguf/27B/M-kv-bias.gguf");
        assert_eq!(find_kv_bias(model.parent().unwrap()), Some(bias.clone()));
        assert_eq!(find_kv_bias(&root), None);
        let install = BonsaiInstall::detect(&root).unwrap();
        assert!(install.launcher_ps1.is_none() && install.server_exe.as_ref() == Some(&exe));
        let mgr = ServerManager::with_log_path(install, "http://127.0.0.1:8080", root.join("x.log"));
        let opts = |pairs: &[(&str, &str)]| StartOptions {
            env: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..StartOptions::default()
        };
        let launch = mgr.launch_command(&opts(&[("BONSAI_KV4", "1"), ("BONSAI_SPECULATIVE", "1")])).unwrap();
        assert!(launch.direct);
        assert!(
            launch.shown.contains("--cache-type-k q4_0") && launch.shown.contains("--kv-mean-center"),
            "{}",
            launch.shown
        );
        assert!(launch.shown.contains("--image-max-tokens 1024"), "Vulkan build: capped: {}", launch.shown);
        assert_eq!(launch.notes, vec!["ignored without the launcher script: BONSAI_SPECULATIVE".to_string()]);
        let rot = launch.cmd.get_envs().find(|(k, _)| *k == "LLAMA_ATTN_ROT_DISABLE").and_then(|(_, v)| v);
        assert_eq!(rot, Some(OsStr::new("1")));
        let uncapped = mgr.launch_command(&opts(&[("BONSAI_IMAGE_MAX_TOKENS", "0")])).unwrap();
        assert!(
            !uncapped.shown.contains("--image-max-tokens") && !uncapped.shown.contains("q4_0"),
            "{}",
            uncapped.shown
        );
        assert!(uncapped.notes.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ports_and_env() {
        assert_eq!(port_from_url("http://127.0.0.1:8080"), Some(8080));
        assert_eq!(port_from_url("http://localhost:8081/"), Some(8081));
        assert_eq!(port_from_url("http://[::1]:9000/v1"), Some(9000));
        assert_eq!(port_from_url("http://localhost"), None);
        assert_eq!(url_host_port("http://user:pw@[::1]:8080/x"), ("::1".to_string(), Some(8080)));
        assert_eq!(
            loopback_addrs("http://127.0.0.1:8080"),
            Some(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)])
        );
        assert_eq!(loopback_addrs("http://localhost:8081/").map(|a| a.len()), Some(2));
        assert_eq!(
            loopback_addrs("http://[::1]:9000"),
            Some(vec![SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9000)])
        );
        assert_eq!(loopback_addrs("http://127.0.0.1").map(|a| a[0].port()), Some(80));
        assert_eq!(loopback_addrs("http://192.168.1.5:8080"), None);
        assert_eq!(loopback_addrs("http://example.com:8080"), None);
        let opts = StartOptions {
            env: vec![("bonsai_ctx".into(), "8192".into()), ("BONSAI_CTX".into(), "4096".into())],
            ..StartOptions::default()
        };
        assert_eq!(env_value(&opts, "BONSAI_CTX").as_deref(), Some("4096"));
        assert_eq!(
            describe_env(&[("BONSAI_KV4".into(), "1".into()), ("HF_TOKEN".into(), "secret".into())]),
            "BONSAI_KV4=1, HF_TOKEN=***"
        );
    }

    #[test]
    fn detects_synthetic_install() {
        let root = temp_dir("install");
        touch_file(&root, "scripts/start_llama_server.ps1");
        touch_file(&root, "start_bonsai.bat");
        let exe = touch_file(&root, &format!("bin/vulkan/{SERVER_EXE_NAME}"));
        touch_file(&root, "bin/cuda/llama-server-impl.dll");
        let ptq = touch_file(&root, "models/bonsai2-gguf/27B/Ternary-Bonsai-2-27B-PTQ1_0.gguf");
        let pq2 = touch_file(&root, "models/bonsai2-gguf/27B/Ternary-Bonsai-2-27B-PQ2_0.gguf");
        let mm = touch_file(&root, "models/bonsai2-gguf/27B/Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf");
        touch_file(&root, "models/other/A-mmproj.gguf");
        touch_file(&root, "models/other/A-dspark-dflash.gguf");
        touch_file(&root, "models/other/A-kv-bias.gguf");
        let other = touch_file(&root, "models/other/A-Q1_0.gguf");
        touch_file(&root, "models/bonsai2-gguf/27B/.cache/partial.gguf");
        touch_file(&root, "models/bonsai2-gguf/27B/notes.txt");

        let install = BonsaiInstall::detect(&root).unwrap();
        assert_eq!(install.root, root);
        assert!(install.launcher_ps1.is_some() && install.launcher_bat.is_some());
        assert_eq!(install.server_exe.as_ref(), Some(&exe), "bin\\cuda has no exe, so vulkan");
        assert_eq!(install.backend(), Some("Vulkan"));
        assert_eq!(install.models.len(), 3, "{:?}", install.models);
        assert!(install.models.contains(&ptq) && install.models.contains(&pq2) && install.models.contains(&other));
        assert_eq!(install.primary_model(), Some(&pq2), "PQ2_0 first, like the launcher");
        assert_eq!(install.mmproj.as_ref(), Some(&mm), "the projector next to the model");
        assert!(install.is_usable(), "{:?}", install.problems());
        assert!(install.warnings().is_empty(), "{:?}", install.warnings());
        let summary = install.summary();
        assert!(summary.contains("✔ Launcher"), "{summary}");
        assert!(summary.contains("Ternary-Bonsai-2-27B-PQ2_0.gguf"), "{summary}");
        assert!(summary.contains("+ 2 more model file(s)"), "{summary}");
        assert!(summary.contains("Ready"), "{summary}");

        // Picking the scripts folder by mistake.
        let from_scripts = BonsaiInstall::detect(&root.join("scripts")).unwrap();
        assert_eq!(from_scripts.root, root);

        // Without PQ2_0 the PTQ1_0 model is picked.
        std::fs::remove_file(&pq2).unwrap();
        assert_eq!(BonsaiInstall::detect(&root).unwrap().primary_model(), Some(&ptq));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn detect_partial_installs() {
        // Launcher only: not usable (no exe, no model).
        let root = temp_dir("partial");
        touch_file(&root, "scripts/start_llama_server.ps1");
        let install = BonsaiInstall::detect(&root).unwrap();
        assert!(!install.is_usable());
        let problems = install.problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("llama-server.exe not found"));
        assert!(problems[1].contains("No model"));
        assert!(install.summary().contains("❌ Server"));
        let _ = std::fs::remove_dir_all(&root);

        // Exe + model without the launcher: usable (direct start), with a warning.
        let root = temp_dir("direct");
        touch_file(&root, &format!("bin/cuda/{SERVER_EXE_NAME}"));
        touch_file(&root, "models/gguf/8B/Bonsai-8B-Q1_0.gguf");
        let install = BonsaiInstall::detect(&root).unwrap();
        assert!(install.is_usable(), "{:?}", install.problems());
        assert!(install.warnings().iter().any(|w| w.contains("started directly")));
        assert!(install.warnings().iter().any(|w| w.contains("vision projector")));
        assert!(install.summary().contains("⚠ Launcher"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_rejects_non_installs() {
        let root = temp_dir("empty");
        touch_file(&root, "models/x.gguf");
        let err = BonsaiInstall::detect(&root).unwrap_err().to_string();
        assert!(err.contains("not a Bonsai-demo folder"), "{err}");
        let missing = root.join("does-not-exist");
        assert!(BonsaiInstall::detect(&missing).is_err());
        let _ = std::fs::remove_dir_all(root);
        assert!(!BonsaiInstall::default().is_usable());
    }

    /// Read-only check of the real install; skipped when `D:\Bonsai-demo` is missing.
    #[test]
    fn detects_real_bonsai_demo() {
        let root = Path::new(r"D:\Bonsai-demo");
        if !root.join("scripts").join("start_llama_server.ps1").is_file() {
            eprintln!("D:\\Bonsai-demo not found - skipped");
            return;
        }
        let install = BonsaiInstall::detect(root).unwrap();
        println!("{}", install.summary());
        assert!(install.launcher_ps1.is_some());
        assert!(install.server_exe.is_some());
        assert!(install.is_usable(), "{:?}", install.problems());
        let model = install.primary_model().unwrap();
        let name = model.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(name.ends_with("-pq2_0.gguf") || name.ends_with("-ptq1_0.gguf"), "{name}");
        assert!(install.models.iter().all(|m| !file_name_lower(m).contains("mmproj")));
        assert!(candidate_dirs().iter().any(|d| same_path(d, root)));
        assert!(find_installs().iter().any(|i| same_path(&i.root, root)));
    }

    #[test]
    fn candidate_dirs_exist_and_are_unique() {
        let dirs = candidate_dirs();
        let mut keys = HashSet::new();
        for d in &dirs {
            assert!(d.is_dir(), "{}", d.display());
            assert!(keys.insert(path_key(d)), "duplicate {}", d.display());
        }
    }

    #[test]
    fn path_helpers() {
        assert!(same_path(Path::new("D:/Bonsai-demo/"), Path::new(r"D:\Bonsai-demo")) || !cfg!(windows));
        if cfg!(windows) {
            assert!(same_path(Path::new(r"d:\bonsai-DEMO"), Path::new(r"D:\Bonsai-demo")));
        }
        assert_eq!(fmt_bytes(5_800_000_000), "5.4 GB");
        assert_eq!(fmt_bytes(660_000_000), "629 MB");
        assert_eq!(model_rank(Path::new("X-PQ2_0.gguf")), 0);
        assert_eq!(model_rank(Path::new("X-ptq1_0.GGUF")), 1);
        assert_eq!(model_rank(Path::new("X-Q1_0.gguf")), 2);
    }

    /// Our own dummy process tree (never llama-server): `taskkill /F /T /PID` ends it.
    #[cfg(windows)]
    #[test]
    fn kills_process_tree() {
        let mut cmd = Command::new("cmd.exe");
        cmd.args(["/C", "ping -n 30 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        hide_console(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(matches!(child.try_wait(), Ok(None)));
        kill_process_tree(child.id()).unwrap();
        wait_child_exit(&mut child, Duration::from_secs(5));
        assert!(matches!(child.try_wait(), Ok(Some(_))), "process tree still running");
        // Already gone: still fine.
        assert!(kill_process_tree(child.id()).is_ok());
    }

    #[test]
    fn run_hidden_captures_and_times_out() {
        let (prog, args): (&str, Vec<&str>) =
            if cfg!(windows) { ("cmd.exe", vec!["/C", "echo hello"]) } else { ("sh", vec!["-c", "echo hello"]) };
        let out = run_hidden(prog, args, Duration::from_secs(10)).unwrap();
        assert!(out.success);
        assert_eq!(out.stdout.trim(), "hello");
        // A single process (no shell), so the kill after the timeout ends it.
        let (prog, args): (&str, Vec<&str>) =
            if cfg!(windows) { ("ping", vec!["-n", "20", "127.0.0.1"]) } else { ("sleep", vec!["20"]) };
        let started = Instant::now();
        let out = run_hidden(prog, args, Duration::from_millis(300)).unwrap();
        assert!(!out.success && out.code.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(run_hidden("bh3-no-such-program-xyz", ["x"], Duration::from_secs(1)).is_err());
    }

    // ---- Lifecycle with a fake install on a free port. Only the fake launcher's own process tree
    // is ever killed; `stop()` runs only after checking that it targets nothing but that.

    /// Kills the fake launcher's process tree when the test ends (also on failure).
    #[cfg_attr(not(windows), allow(dead_code))]
    struct KillOnDrop(Option<u32>);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                let _ = kill_process_tree(pid);
            }
        }
    }

    /// A port nothing listens on (connection refused).
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Fake install whose launcher script has `script` as its body.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn fake_install(name: &str, script: &str) -> (PathBuf, BonsaiInstall) {
        let root = temp_dir(name);
        let ps1 = touch_file(&root, "scripts/start_llama_server.ps1");
        std::fs::write(ps1, script).unwrap();
        touch_file(&root, &format!("bin/cuda/{SERVER_EXE_NAME}"));
        touch_file(&root, "models/bonsai2-gguf/27B/Fake-PTQ1_0.gguf");
        let install = BonsaiInstall::detect(&root).unwrap();
        (root, install)
    }

    /// Minimal `/health` responder in this test process: `down` answers 500, then `loading`
    /// answers 503, then 200, until `stop` is set. Bare connects (the loopback probe) get nothing.
    #[cfg_attr(not(windows), allow(dead_code))]
    struct FakeHealth {
        stop: std::sync::Arc<AtomicBool>,
        port: u16,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    impl FakeHealth {
        fn start(port: u16, loading: usize) -> Self {
            Self::start_with(port, 0, loading)
        }

        fn start_with(port: u16, down: usize, loading: usize) -> Self {
            let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let thread = std::thread::spawn(move || {
                let mut served = 0usize;
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let mut buf = [0u8; 4096];
                    if stream.read(&mut buf).unwrap_or(0) == 0 {
                        continue;
                    }
                    let (status, body) = if served < down {
                        ("500 Internal Server Error", r#"{"error":{"code":500,"message":"busy"}}"#)
                    } else if served < down + loading {
                        ("503 Service Unavailable", r#"{"error":{"code":503,"message":"Loading model"}}"#)
                    } else {
                        ("200 OK", r#"{"status":"ok"}"#)
                    };
                    served += 1;
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                }
            });
            Self { stop, port, thread: Some(thread) }
        }

        fn shutdown(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    impl Drop for FakeHealth {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn forced(env: &[(&str, &str)]) -> StartOptions {
        StartOptions {
            env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            force: true,
            vram_busy_mb: 0,
        }
    }

    #[cfg(windows)]
    #[test]
    fn lifecycle_launcher_failure_becomes_failed() {
        let script = "Write-Host \"ctx=$env:BONSAI_CTX\"\r\n\
                      Write-Host \"cwd=$((Get-Location).Path)\"\r\n\
                      Write-Host \"[ERR] No model file found for Bonsai-2-27B in X\" -ForegroundColor Red\r\n\
                      exit 1\r\n";
        let (root, install) = fake_install("life-fail", script);
        let url = format!("http://127.0.0.1:{}", free_port());
        let log = root.join("logs").join("server.log");
        let mut mgr = ServerManager::with_log_path(install, &url, log.clone());
        assert_eq!(mgr.refresh(), &ServerState::Stopped);

        mgr.start(&forced(&[("BONSAI_CTX", "4096")])).unwrap();
        let _guard = KillOnDrop(mgr.child_pid());
        assert_eq!(mgr.state(), &ServerState::Starting);
        assert!(mgr.started_by_us() && mgr.uptime().is_some());

        let mut seen = Vec::new();
        let err = mgr
            .wait_ready(Duration::from_secs(60), &AtomicBool::new(false), &mut |s| seen.push(s.clone()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("[ERR] No model file found for Bonsai-2-27B in X"), "{err}");
        assert_eq!(mgr.state(), &ServerState::Failed("[ERR] No model file found for Bonsai-2-27B in X".into()));
        assert!(matches!(seen.last(), Some(ServerState::Failed(_))), "{seen:?}");
        assert!(!mgr.started_by_us() && mgr.uptime().is_none());
        // Failed sticks while nothing answers.
        assert!(matches!(mgr.refresh(), ServerState::Failed(_)));

        let tail = mgr.log_tail(50);
        assert!(tail.iter().any(|l| l.starts_with("=== ") && l.ends_with(" start ===")), "{tail:?}");
        assert!(tail.iter().any(|l| l == "ctx=4096"), "env passed: {tail:?}");
        // Compared as resolved on disk: TEMP can be an 8.3 short path (`C:\Users\RUNNER~1\...` on
        // CI runners) while PowerShell prints the long one.
        let real = |p: &Path| std::fs::canonicalize(p).map(|c| path_key(&c)).unwrap_or_else(|_| path_key(p));
        let want = real(&root);
        let cwd_ok = tail.iter().filter_map(|l| l.strip_prefix("cwd=")).any(|l| real(Path::new(l)) == want);
        assert!(cwd_ok, "working dir {}: {tail:?}", root.display());
        assert!(tail.iter().any(|l| l.starts_with("[bh3] env: BONSAI_CTX=4096")), "{tail:?}");
        drop(_guard);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn lifecycle_ensure_running_reaches_ready() {
        let (root, install) = fake_install("life-ready", "Write-Host 'fake launcher'\r\nStart-Sleep -Seconds 120\r\n");
        let port = free_port();
        let url = format!("http://127.0.0.1:{port}");
        let mut mgr = ServerManager::with_log_path(install, &url, root.join("server.log"));

        // The fake server comes up once the launcher runs (as llama-server would): 503 twice, then 200.
        let mut fake: Option<FakeHealth> = None;
        let mut seen = Vec::new();
        let started = mgr
            .ensure_running(&forced(&[]), Duration::from_secs(60), &AtomicBool::new(false), &mut |s| {
                seen.push(s.clone());
                if fake.is_none() {
                    fake = Some(FakeHealth::start(port, 2));
                }
            })
            .unwrap();
        let mut guard = KillOnDrop(mgr.child_pid());
        assert!(started, "started now");
        assert_eq!(seen.first(), Some(&ServerState::Starting), "{seen:?}");
        assert!(seen.contains(&ServerState::Loading), "{seen:?}");
        assert_eq!(seen.last(), Some(&ServerState::Ready), "{seen:?}");
        assert!(mgr.state().is_ready() && mgr.started_by_us());
        assert!(mgr.idle_for() < Duration::from_secs(5));

        // Already ready: nothing to start.
        let again =
            mgr.ensure_running(&forced(&[]), Duration::from_secs(5), &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert!(!again);
        assert!(mgr.start(&forced(&[])).unwrap_err().to_string().contains("already running"));

        // Our launcher gone but something still answers: Ready, not ours any more.
        if let Some(pid) = guard.0.take() {
            kill_process_tree(pid).unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(mgr.refresh(), &ServerState::Ready);
        assert!(!mgr.started_by_us() && mgr.child_pid().is_none());

        // The server goes away: one tolerated miss, then Stopped.
        if let Some(mut f) = fake.take() {
            f.shutdown();
        }
        assert_eq!(mgr.refresh(), &ServerState::Ready);
        assert_eq!(mgr.refresh(), &ServerState::Stopped);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn lifecycle_cancel_and_timeout() {
        let (root, install) = fake_install("life-cancel", "Start-Sleep -Seconds 120\r\n");
        let url = format!("http://127.0.0.1:{}", free_port());
        let mut mgr = ServerManager::with_log_path(install, &url, root.join("server.log"));
        // Cancel as soon as the launcher runs (the user pressing Esc while Bonsai starts).
        let cancel = AtomicBool::new(false);
        let err = mgr
            .ensure_running(&forced(&[]), Duration::from_secs(60), &cancel, &mut |s| {
                if *s == ServerState::Starting {
                    cancel.store(true, Ordering::SeqCst);
                }
            })
            .unwrap_err();
        let _guard = KillOnDrop(mgr.child_pid());
        assert!(err.downcast_ref::<WaitCancelled>().is_some(), "{err:#}");
        assert_eq!(mgr.state(), &ServerState::Starting, "the launcher keeps running after a cancel");
        assert!(mgr.child_pid().is_some() && mgr.started_by_us());
        let again = mgr.start(&forced(&[])).unwrap_err().to_string();
        assert!(again.contains("already starting"), "{again}");
        // An already set cancel flag stops before anything is started.
        let mut idle = ServerManager::with_log_path(mgr.install().clone(), &url, root.join("other.log"));
        let err =
            idle.ensure_running(&forced(&[]), Duration::from_secs(5), &AtomicBool::new(true), &mut |_| {}).unwrap_err();
        assert!(err.downcast_ref::<WaitCancelled>().is_some() && idle.child_pid().is_none());

        let started = Instant::now();
        let err = mgr.wait_ready(Duration::from_secs(1), &AtomicBool::new(false), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("did not become ready within 1 s"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(6));
        drop(_guard);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A server that answers just as the start begins (start_bonsai.bat, another bh3) is waited
    /// for: Ok(false), nothing spawned.
    #[cfg(windows)]
    #[test]
    fn ensure_running_waits_for_a_server_that_appears() {
        let root = temp_dir("appears");
        let install = BonsaiInstall { root: root.clone(), ..BonsaiInstall::default() };
        let port = free_port();
        // The first /health fails (state Stopped), the next ones answer 200.
        let _fake = FakeHealth::start_with(port, 1, 0);
        let mut mgr = ServerManager::with_log_path(install, &format!("http://127.0.0.1:{port}"), root.join("x.log"));
        let mut seen = Vec::new();
        let started = mgr
            .ensure_running(&StartOptions::default(), Duration::from_secs(10), &AtomicBool::new(false), &mut |s| {
                seen.push(s.clone())
            })
            .unwrap();
        assert!(!started, "not ours: {seen:?}");
        assert!(mgr.state().is_ready() && !mgr.started_by_us() && mgr.child_pid().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    /// The netstat lookup on a real socket: our own test process listens, which is not a
    /// llama-server, so nothing would be stopped.
    #[cfg(windows)]
    #[test]
    fn port_listener_is_found_but_only_llama_server_counts() {
        let port = free_port();
        let _fake = FakeHealth::start(port, 0);
        assert!(listening_pids(port).contains(&std::process::id()), "{:?}", listening_pids(port));
        let root = temp_dir("listener");
        let install = BonsaiInstall { root: root.clone(), ..BonsaiInstall::default() };
        let mgr = ServerManager::with_log_path(install, &format!("http://127.0.0.1:{port}"), root.join("x.log"));
        assert!(mgr.port_server_pids().is_empty());
        assert!(!mgr.server_pids().contains(&std::process::id()));
        let remote =
            ServerManager::with_log_path(BonsaiInstall::default(), "http://192.168.1.5:8080", root.join("y.log"));
        assert!(remote.port_server_pids().is_empty(), "not on this machine");
        let _ = std::fs::remove_dir_all(root);
    }

    /// `stop()` on our own fake launcher (free port, nothing listening): only its tree is killed.
    #[cfg(windows)]
    #[test]
    fn lifecycle_stop_kills_only_our_launcher() {
        let (root, install) = fake_install("life-stop", "Start-Sleep -Seconds 120\r\n");
        let url = format!("http://127.0.0.1:{}", free_port());
        let mut mgr = ServerManager::with_log_path(install, &url, root.join("server.log"));
        mgr.start(&forced(&[])).unwrap();
        let mut guard = KillOnDrop(mgr.child_pid());
        let launcher = guard.0.unwrap();
        // Safety net: nothing but our launcher may be a target.
        assert!(mgr.port_server_pids().is_empty(), "{:?}", mgr.port_server_pids());
        let others_before = llama_server_pids();
        mgr.stop().unwrap();
        assert_eq!(mgr.state(), &ServerState::Stopped);
        assert!(mgr.child_pid().is_none() && !mgr.started_by_us());
        let filter = format!("PID eq {launcher}");
        let gone = run_hidden("tasklist", ["/FI", filter.as_str(), "/FO", "CSV", "/NH"], TOOL_TIMEOUT);
        assert!(gone.is_ok_and(|out| !out.stdout.contains(&format!("\"{launcher}\""))), "launcher still runs");
        for pid in others_before {
            assert!(llama_server_pids().contains(&pid), "llama-server {pid} on another port was touched");
        }
        guard.0 = None;
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn wait_ready_without_anything_starting_errors() {
        let root = temp_dir("nothing");
        let install = BonsaiInstall { root: root.clone(), ..BonsaiInstall::default() };
        let url = format!("http://127.0.0.1:{}", free_port());
        let mut mgr = ServerManager::with_log_path(install, &url, root.join("server.log"));
        let begun = Instant::now();
        let res = mgr.wait_ready(Duration::from_secs(20), &AtomicBool::new(false), &mut |_| {});
        // A llama-server on another port (e.g. the user's on 8080) does not make it wait.
        let err = res.unwrap_err().to_string();
        assert!(err.contains("nothing is starting it"), "{err}");
        assert!(begun.elapsed() < Duration::from_secs(10));
        assert!(mgr.log_tail(10).is_empty());
        let start_err = mgr.start(&StartOptions::default()).unwrap_err().to_string();
        assert!(start_err.contains("not a Bonsai-demo folder"), "{start_err}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Live (read-only): lists llama-server PIDs and the GPU. `cargo test --lib server:: -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_pids_and_gpu() {
        println!("llama-server PIDs: {:?}", llama_server_pids());
        println!("GPU: {:#?}", crate::gpu::query());
        println!("installs: {:#?}", find_installs().iter().map(|i| i.root.clone()).collect::<Vec<_>>());
    }
}
