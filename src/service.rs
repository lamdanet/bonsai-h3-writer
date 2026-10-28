//! Background service for the GUI: owns the [`ServerManager`], runs jobs on worker threads and
//! reports through events, so the UI thread never blocks.
//!
//! Behaviour:
//! - The control thread waits on the command channel (~200 ms timeout) and does periodic work:
//!   status refresh about every 2 s (every 1 s while Bonsai starts; `ServerManager::refresh`,
//!   `/props` once per Ready transition), log tail (last 200 lines) about every 2 s. The GPU is
//!   polled about every 3 s on its own small thread. A [`StatusSnapshot`] goes out when
//!   something changed and at least every second.
//! - `Submit`: the job is queued (`JobQueued`). Jobs run one at a time (queue), each on a worker
//!   thread with its own cancel flag; deltas are batched (~every 50 ms) before `JobDelta`; phases
//!   ("Processing prompt...", "Thinking...", "Writing...") arrive as `JobPhase`. Write jobs load
//!   the library fresh (so Library edits apply) and finish through [`crate::writer::finish`].
//!   Every job calls `touch()` when it starts and ends. Requests are always streamed (so they can
//!   be cancelled), whatever `GenParams::stream` says.
//! - A job waits while the server is not Ready: no usable install -> `JobFailed("Choose the
//!   Bonsai folder first ...")`; auto-start off -> `JobFailed("Bonsai is not running ...")`;
//!   otherwise `ServerManager::start` (non-forced: refuses while other programs use VRAM, without
//!   blocking) and phases "Starting Bonsai..." / "Loading model..." until Ready, a failed start
//!   (`JobFailed` with the reason) or `start_timeout_secs`. A start begun elsewhere (Start button,
//!   `start_bonsai.bat`) is waited for the same way.
//! - `StartServer` is forced; `StopServer` cancels every job first; `RestartServer` cancels the
//!   running job and keeps the queue (it runs after the restart) - unless no usable install could
//!   start Bonsai again (then nothing is stopped and an `Error` says why). A stop runs on a helper
//!   thread that takes the manager along and hands it back, so the control loop stays responsive.
//! - Idle auto-stop: when started_by_us, the server is Ready, no job runs or waits and idle >=
//!   `auto_stop_idle_minutes` -> stop + `Notice`. Other programs may use the same server (its web
//!   UI, `bh3`, another app instance), so while the countdown runs `/slots` is checked about every
//!   20 s on a helper thread: a processing slot or a new task counts as activity. Right before the
//!   stop one more check runs; a busy server, or one that does not answer it, postpones the stop.
//! - `SetInstall` and a new server URL in `UpdateConfig` rebuild the manager, but not while a job
//!   runs or waits for a start, and not while a server this app started is still up (idle
//!   auto-stop and stop-on-exit need the manager that started it). The change then applies as
//!   soon as that is over (a Stop applies it at once); the snapshot shows the new install at once.
//!   `UpdateConfig` never changes the install: send `SetInstall` for that.
//! - `Shutdown`: cancel jobs; stop the server if `stop_on_exit` and started_by_us; send
//!   `ShutdownComplete`; end the thread. A [`ServiceHandle`] dropped without a `Shutdown` (e.g.
//!   the UI panicked) sends one and waits for it, up to 15 s.
//! - After every event sent, call the `repaint` callback.

use std::any::Any;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::client::{BonsaiClient, ChatResult, ServerProps, SlotsActivity, StreamEvent};
use crate::config::AppConfig;
use crate::gen::{ChatMessage, GenParams, ThinkingMode};
use crate::gpu::{self, GpuInfo};
use crate::library::Library;
use crate::server::{BonsaiInstall, ServerManager, ServerState, StartOptions};
use crate::writer::{self, WriteJob, WriteOutcome};

/// Id the UI gives each job.
pub type JobId = u64;

/// Work for Bonsai.
// Variants are the public contract (a `WriteJob` is moved once per job): not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum JobKind {
    /// Write (or fix) an H3 prompt.
    Write(WriteJob),
    /// Free chat: full message list (system prompt included by the caller when wanted).
    Chat { messages: Vec<ChatMessage>, params: GenParams },
}

/// UI -> service.
// Variants are the public contract (`Submit` carries the job): not boxed.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Command {
    /// Refresh status now.
    Refresh,
    /// Manual start (forced: starts even when other programs use VRAM).
    StartServer,
    /// Stop the server (also one started elsewhere); cancels all jobs first.
    StopServer,
    /// Stop, then start (forced); the running job is cancelled, queued jobs run afterwards.
    RestartServer,
    /// Queue a job; answered with `JobQueued`, then phases, deltas and one final event.
    Submit { id: JobId, kind: JobKind },
    /// Cancel a queued or running job (answered with `JobCancelled`).
    Cancel(JobId),
    /// Count tokens of a text (`/tokenize`); answered with `TokenCount`. Needs the server Ready.
    CountTokens { id: JobId, text: String },
    /// New settings (server URL, auto-start, idle minutes, launcher env ...).
    UpdateConfig(Box<AppConfig>),
    /// New install folder (or none).
    SetInstall(Option<BonsaiInstall>),
    /// Cancel everything, stop the server if the settings say so, then end the service.
    Shutdown,
}

/// Everything the status bar / server view shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusSnapshot {
    /// Server lifecycle (`Stopping` while a stop runs).
    pub state: ServerState,
    /// This app started the running server (idle auto-stop and stop-on-exit apply).
    pub started_by_us: bool,
    /// Install configured and usable.
    pub install_ok: bool,
    /// The chosen install (`None` when no folder is set).
    pub install: Option<BonsaiInstall>,
    /// Model and server facts, fetched once per Ready transition (`None` unless Ready).
    pub props: Option<ServerProps>,
    /// Latest GPU poll (`None` without nvidia-smi).
    pub gpu: Option<GpuInfo>,
    /// Seconds since the last activity: this app's jobs, or other programs' requests seen in
    /// `/slots` while the auto-stop counts down.
    pub idle_secs: u64,
    /// Seconds until idle auto-stop (when it applies).
    pub auto_stop_in_secs: Option<u64>,
    /// Seconds since this app started the server.
    pub uptime_secs: Option<u64>,
    /// A job is running.
    pub busy: bool,
    /// Jobs waiting.
    pub queued: usize,
    /// Last lines of the server log.
    pub log_tail: Vec<String>,
}

/// Service -> UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Server, GPU and queue status (sent on changes and about every second).
    Status(Box<StatusSnapshot>),
    /// Job accepted (queued or running).
    JobQueued(JobId),
    /// e.g. "Starting Bonsai...", "Loading model...", "Thinking...", "Writing..." (see [`phase`]).
    JobPhase { id: JobId, phase: String },
    /// New text since the last delta.
    JobDelta { id: JobId, reasoning: String, content: String },
    /// A write job finished.
    WriteDone { id: JobId, outcome: Box<WriteOutcome> },
    /// A chat job finished.
    ChatDone { id: JobId, result: Box<ChatResult> },
    /// A job failed; `error` is ready to show.
    JobFailed { id: JobId, error: String },
    /// A job was cancelled (queued or running).
    JobCancelled(JobId),
    /// Answer to `CountTokens`.
    TokenCount { id: JobId, count: usize },
    /// Info for a toast (e.g. "Bonsai stopped after 10 idle minutes").
    Notice(String),
    /// Error for a toast (e.g. a failed manual start).
    Error(String),
    /// Sent once after `Shutdown` finished (server stopped if it had to be); the thread then ends.
    ShutdownComplete,
}

/// Texts sent in [`Event::JobPhase`] (public so the UI can recognise them).
pub mod phase {
    /// Queued behind the running job.
    pub const QUEUED: &str = "Waiting for the previous job...";
    /// A stop (manual, restart or idle) is in progress; the job continues after it.
    pub const WAIT_STOP: &str = "Waiting for Bonsai to stop...";
    /// The launcher runs; nothing answers yet.
    pub const STARTING: &str = "Starting Bonsai...";
    /// llama-server answers 503 while it loads the model.
    pub const LOADING: &str = "Loading model...";
    /// Request sent; Bonsai reads the prompt (and images) before the first token.
    pub const PROMPT: &str = "Processing prompt...";
    /// The first reasoning text arrived.
    pub const THINKING: &str = "Thinking...";
    /// The first answer text arrived.
    pub const WRITING: &str = "Writing...";
    /// Cancel requested for the running job; it ends at the next token.
    pub const CANCELLING: &str = "Cancelling...";
}

/// UI side of the service.
pub struct ServiceHandle {
    tx: Sender<Command>,
    rx: Receiver<Event>,
    /// A `Shutdown` was sent (then dropping the handle does not wait again).
    shutdown_sent: Cell<bool>,
}

impl ServiceHandle {
    /// Send a command (ignored when the service has ended).
    pub fn send(&self, cmd: Command) {
        if matches!(cmd, Command::Shutdown) {
            self.shutdown_sent.set(true);
        }
        let _ = self.tx.send(cmd);
    }

    /// Next event without blocking.
    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// All pending events.
    pub fn drain(&self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Block until `ShutdownComplete` arrives, the service thread is gone, or `timeout` passes.
    /// Returns true when the service confirmed the shutdown.
    pub fn wait_shutdown(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.rx.recv_timeout(left) {
                Ok(Event::ShutdownComplete) => return true,
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return false,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return true,
            }
        }
    }
}

impl Drop for ServiceHandle {
    /// A handle dropped without a `Shutdown` (the UI ended without its exit hook, e.g. after a
    /// panic) still shuts the service down and waits for it, so a Bonsai this app started is
    /// stopped as `stop_on_exit` says.
    fn drop(&mut self) {
        if !self.shutdown_sent.get() {
            self.send(Command::Shutdown);
            self.wait_shutdown(DROP_SHUTDOWN_WAIT);
        }
    }
}

/// Start the service thread. `repaint` is called after events are sent (egui `request_repaint`).
///
/// Returns at once: install detection (`config.bonsai_dir`) and the first status check run on
/// the service thread.
pub fn spawn(config: AppConfig, repaint: Arc<dyn Fn() + Send + Sync>) -> ServiceHandle {
    let (tx, commands) = mpsc::channel();
    let (event_tx, rx) = mpsc::channel();
    let out = Outbox { tx: event_tx, repaint };
    let crash_out = out.clone();
    let spawn_failed_out = out.clone();
    let spawned = thread::Builder::new().name("bh3-service".into()).spawn(move || {
        let ran = panic::catch_unwind(AssertUnwindSafe(|| Service::new(config, out).run(&commands)));
        if let Err(payload) = ran {
            crash_out.error(format!("The background service stopped: {} - restart the app", panic_message(payload)));
        }
    });
    if let Err(e) = spawned {
        spawn_failed_out.error(format!("Could not start the background service: {e}"));
    }
    ServiceHandle { tx, rx, shutdown_sent: Cell::new(false) }
}

// ---------------------------------------------------------------------------------------------
// Timing and messages

/// The control loop wakes up at least this often.
const TICK: Duration = Duration::from_millis(200);
/// A status snapshot goes out at least this often.
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// Server state refresh.
const REFRESH_EVERY: Duration = Duration::from_secs(2);
/// Server state refresh while it starts or loads, or while jobs wait for it.
const REFRESH_FAST: Duration = Duration::from_secs(1);
/// Log tail check (cheap: only re-read when the file changed).
const LOG_EVERY: Duration = Duration::from_secs(2);
/// Re-read the log even when its size and time look unchanged.
const LOG_FORCE_EVERY: Duration = Duration::from_secs(10);
/// Log lines in the snapshot.
const LOG_LINES: usize = 200;
/// GPU poll.
const GPU_EVERY: Duration = Duration::from_secs(3);
/// GPU poll after nvidia-smi failed several times in a row (no NVIDIA GPU / driver).
const GPU_EVERY_MISSING: Duration = Duration::from_secs(30);
/// Streamed text is batched this long before `JobDelta`.
const DELTA_EVERY: Duration = Duration::from_millis(50);
/// After our own launch, a `Stopped` state this young means "not up yet", not a failed start.
const START_GRACE: Duration = Duration::from_secs(5);
/// Used when `start_timeout_secs` is 0.
const DEFAULT_START_TIMEOUT_SECS: u32 = 300;
/// On shutdown: wait this long for the cancelled job to end ...
const SHUTDOWN_JOB_WAIT: Duration = Duration::from_secs(2);
/// ... and this long for a stop that is already in progress.
const SHUTDOWN_STOP_WAIT: Duration = Duration::from_secs(12);
/// A dropped [`ServiceHandle`] that never sent `Shutdown` waits this long for it (the app's exit
/// hook waits as long).
const DROP_SHUTDOWN_WAIT: Duration = Duration::from_secs(15);
/// `/slots` check interval while the idle auto-stop counts down (other programs' requests).
const SLOTS_EVERY: Duration = Duration::from_secs(20);
/// Whole-request timeout of one `/slots` check (a busy server answers between two batches).
const SLOTS_TIMEOUT: Duration = Duration::from_secs(3);

/// Job error when no usable Bonsai folder is configured.
const NO_INSTALL: &str = "Choose the Bonsai folder first (Bonsai workspace)";
/// Job error when the server is down and auto-start is off.
const NOT_RUNNING: &str = "Bonsai is not running - press Start or enable auto-start";

// ---------------------------------------------------------------------------------------------
// Plumbing shared by the threads

/// Event sender that wakes the UI after every event.
#[derive(Clone)]
struct Outbox {
    tx: Sender<Event>,
    repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Outbox {
    fn send(&self, ev: Event) {
        // The UI may be gone (closing); nothing to do then.
        let _ = self.tx.send(ev);
        (self.repaint)();
    }

    fn notice(&self, text: impl Into<String>) {
        self.send(Event::Notice(text.into()));
    }

    fn error(&self, text: impl Into<String>) {
        self.send(Event::Error(text.into()));
    }
}

/// Helper threads -> control thread.
enum Internal {
    /// A job thread sent its final event.
    JobFinished(JobId),
    /// The stop helper is done and hands the manager back.
    StopDone { manager: Box<ServerManager>, result: Result<(), String>, reason: StopReason },
    /// GPU poll result.
    Gpu(Option<GpuInfo>),
    /// `/slots` check result; `last`: the check right before an idle stop.
    Slots { result: Result<Option<SlotsActivity>, String>, last: bool },
}

/// Why the server is being stopped (decides the toast afterwards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    Manual,
    Restart,
    /// Idle auto-stop after this many minutes.
    Idle(u32),
}

/// A stop running on the helper thread.
struct StopInProgress {
    /// `started_by_us` of the manager when the stop began (shown while it is away).
    started_by_us: bool,
}

/// A job waiting to run.
struct QueuedJob {
    id: JobId,
    kind: JobKind,
    /// Last phase sent for it (no repeats).
    phase: Option<&'static str>,
}

/// The job on the worker thread.
struct RunningJob {
    id: JobId,
    cancel: Arc<AtomicBool>,
}

/// Jobs wait for the server to come up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StartWait {
    since: Instant,
    /// This service launched the server for the waiting jobs (else: a start begun elsewhere).
    issued: bool,
}

/// One `/tokenize` request for the token thread.
struct TokenRequest {
    id: JobId,
    text: String,
    client: BonsaiClient,
}

// ---------------------------------------------------------------------------------------------
// Pure decision logic (unit tested)

/// What to do with the waiting jobs when no job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QueueAction {
    /// Run the next job.
    Dispatch,
    /// Keep waiting and show this phase.
    Wait(&'static str),
    /// Fail every waiting job with this message.
    Fail(String),
    /// The server is down and no start is pending: decide whether to start it.
    NeedStart,
}

/// Decide from the server state and the current wait (`now` passed in for tests).
fn queue_action(state: &ServerState, wait: Option<StartWait>, now: Instant, timeout: Duration) -> QueueAction {
    match state {
        ServerState::Ready => QueueAction::Dispatch,
        ServerState::Starting | ServerState::Loading | ServerState::Stopping => {
            let since = wait.map_or(now, |w| w.since);
            if now.saturating_duration_since(since) >= timeout {
                QueueAction::Fail(start_timeout_message(timeout, state))
            } else {
                QueueAction::Wait(waiting_phase(state).unwrap_or(phase::STARTING))
            }
        }
        // A start we waited for (ours or not) failed.
        ServerState::Failed(_) if wait.is_some() => QueueAction::Fail(start_failed_message(state)),
        ServerState::Stopped | ServerState::Failed(_) => match wait {
            Some(w) if w.issued && now.saturating_duration_since(w.since) < START_GRACE => {
                QueueAction::Wait(phase::STARTING)
            }
            Some(w) if w.issued => QueueAction::Fail(start_failed_message(state)),
            // No start pending, or one begun elsewhere went away.
            _ => QueueAction::NeedStart,
        },
    }
}

/// Phase for jobs waiting while the server is in `state` (`None`: not a waiting state).
fn waiting_phase(state: &ServerState) -> Option<&'static str> {
    match state {
        ServerState::Starting => Some(phase::STARTING),
        ServerState::Loading => Some(phase::LOADING),
        ServerState::Stopping => Some(phase::WAIT_STOP),
        _ => None,
    }
}

/// Whether waiting jobs may start the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartDecision {
    Start,
    NoInstall,
    AutoStartOff,
}

/// The install comes first: without one, neither auto-start nor the Start button can help.
fn start_decision(install_usable: bool, auto_start: bool) -> StartDecision {
    if !install_usable {
        StartDecision::NoInstall
    } else if !auto_start {
        StartDecision::AutoStartOff
    } else {
        StartDecision::Start
    }
}

/// Time left until the idle auto-stop, when it applies: this app started the server, it is
/// Ready, no job runs or waits and auto-stop is on (`minutes > 0`).
fn auto_stop_in(
    started_by_us: bool,
    state: &ServerState,
    jobs_active: bool,
    minutes: u32,
    idle: Duration,
) -> Option<Duration> {
    if !started_by_us || !state.is_ready() || jobs_active || minutes == 0 {
        return None;
    }
    Some(Duration::from_secs(u64::from(minutes) * 60).saturating_sub(idle))
}

/// Whether a `/slots` answer counts as activity for the idle auto-stop: a processing slot or a
/// request since `before`. No slots endpoint tells nothing; no answer in time keeps the server
/// up only for the `last` check before the stop (it may be busy with a long prompt).
fn slots_say_busy(result: &Result<Option<SlotsActivity>, String>, before: Option<&SlotsActivity>, last: bool) -> bool {
    match result {
        Ok(Some(activity)) => activity.active_since(before),
        Ok(None) => false,
        Err(_) => last,
    }
}

/// Phase of a running request, from the streamed pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamPhase {
    Thinking,
    Writing,
}

impl StreamPhase {
    fn label(self) -> &'static str {
        match self {
            StreamPhase::Thinking => phase::THINKING,
            StreamPhase::Writing => phase::WRITING,
        }
    }
}

/// The phase `piece` moves the job to, when it changes. Phases only move forward
/// (none -> thinking -> writing); whitespace-only pieces (e.g. the newline after the thinking
/// block) do not count.
fn next_phase(current: Option<StreamPhase>, piece: &StreamEvent) -> Option<StreamPhase> {
    let next = match piece {
        StreamEvent::Reasoning(text) if !text.trim().is_empty() => StreamPhase::Thinking,
        StreamEvent::Content(text) if !text.trim().is_empty() => StreamPhase::Writing,
        _ => return None,
    };
    match current {
        None => Some(next),
        Some(StreamPhase::Thinking) if next == StreamPhase::Writing => Some(next),
        Some(_) => None,
    }
}

/// Collects streamed text and hands it out at most every `interval` (the very first piece at
/// once, so the UI shows the start of the answer without delay).
#[derive(Debug)]
struct DeltaBatcher {
    interval: Duration,
    reasoning: String,
    content: String,
    last_flush: Option<Instant>,
}

impl DeltaBatcher {
    fn new(interval: Duration) -> Self {
        Self { interval, reasoning: String::new(), content: String::new(), last_flush: None }
    }

    fn push(&mut self, piece: StreamEvent) {
        match piece {
            StreamEvent::Reasoning(text) => self.reasoning.push_str(&text),
            StreamEvent::Content(text) => self.content.push_str(&text),
        }
    }

    fn is_empty(&self) -> bool {
        self.reasoning.is_empty() && self.content.is_empty()
    }

    /// `(reasoning, content)` pending, when the interval has passed since the last flush.
    fn take_if_due(&mut self, now: Instant) -> Option<(String, String)> {
        let due = match self.last_flush {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.interval,
        };
        if due {
            self.take(now)
        } else {
            None
        }
    }

    /// Everything pending now (`None` when there is nothing).
    fn take(&mut self, now: Instant) -> Option<(String, String)> {
        if self.is_empty() {
            return None;
        }
        self.last_flush = Some(now);
        Some((std::mem::take(&mut self.reasoning), std::mem::take(&mut self.content)))
    }
}

/// Error for a write job whose cleaned answer is empty.
fn empty_answer_message(finish_reason: &str, params: &GenParams) -> String {
    let reason = match finish_reason.trim() {
        "" => "none",
        other => other,
    };
    let mut msg = format!("Bonsai returned an empty answer (finish_reason {reason})");
    if reason == "length" {
        if params.thinking != ThinkingMode::Off {
            msg.push_str(&format!(
                " - thinking used all {} tokens: turn thinking off or raise max tokens",
                params.max_tokens
            ));
        } else {
            msg.push_str(&format!(" - raise max tokens (now {})", params.max_tokens));
        }
    }
    msg
}

/// Toast after the idle auto-stop.
fn idle_stop_notice(minutes: u32) -> String {
    let unit = if minutes == 1 { "minute" } else { "minutes" };
    format!("Bonsai stopped after {minutes} idle {unit} - GPU free for ComfyUI")
}

/// Job error without a usable install; lists the install's problems when there is one.
fn no_install_message(problems: &[String]) -> String {
    if problems.is_empty() {
        NO_INSTALL.to_string()
    } else {
        format!("{NO_INSTALL}: {}", problems.join("; "))
    }
}

/// Job error when a start the jobs waited for ended without a server.
fn start_failed_message(state: &ServerState) -> String {
    match state {
        ServerState::Failed(reason) if !reason.trim().is_empty() => {
            format!("Bonsai failed to start: {}", reason.trim())
        }
        _ => "Bonsai stopped while starting - see the log in the Bonsai workspace".to_string(),
    }
}

/// Job error when the server did not get ready in time.
fn start_timeout_message(timeout: Duration, state: &ServerState) -> String {
    format!(
        "Bonsai did not get ready within {} s (still {}) - see the log in the Bonsai workspace",
        timeout.as_secs(),
        state.label()
    )
}

/// How long jobs wait for the server (`start_timeout_secs`, 0 = default 300 s).
fn start_timeout(cfg: &AppConfig) -> Duration {
    let secs = if cfg.start_timeout_secs == 0 { DEFAULT_START_TIMEOUT_SECS } else { cfg.start_timeout_secs };
    Duration::from_secs(u64::from(secs))
}

/// GPU poll interval after `misses` failed polls in a row.
fn gpu_poll_interval(misses: u32) -> Duration {
    if misses >= 3 {
        GPU_EVERY_MISSING
    } else {
        GPU_EVERY
    }
}

/// Text of a caught panic.
fn panic_message(payload: Box<dyn Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(text) => *text,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(text) => (*text).to_string(),
            Err(_) => "unknown panic".to_string(),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Control thread

/// State of the control thread.
struct Service {
    config: AppConfig,
    out: Outbox,
    internal_tx: Sender<Internal>,
    internal_rx: Receiver<Internal>,
    /// `None` while a stop runs on the helper thread (the manager travels with it).
    manager: Option<ServerManager>,
    /// Install and URL the current manager was built with.
    built_install: Option<BonsaiInstall>,
    built_url: String,
    /// The install the UI chose (applied to the manager by `reconcile_manager`).
    desired_install: Option<BonsaiInstall>,
    install_ok: bool,
    stopping: Option<StopInProgress>,
    /// Start (forced) once the running stop is done (restart, or Start pressed while stopping).
    start_after_stop: bool,
    queue: VecDeque<QueuedJob>,
    running: Option<RunningJob>,
    start_wait: Option<StartWait>,
    /// Server state at the last refresh.
    last_state: ServerState,
    props: Option<ServerProps>,
    gpu: Option<GpuInfo>,
    gpu_stop: Arc<AtomicBool>,
    log_tail: Vec<String>,
    /// Size and modification time of the log when it was last read.
    log_stamp: Option<(u64, Option<SystemTime>)>,
    log_read_at: Option<Instant>,
    /// Token-count thread (started on first use).
    tokens: Option<Sender<TokenRequest>>,
    /// A `/slots` check runs on a helper thread.
    slots_pending: bool,
    /// When the next periodic `/slots` check is due.
    next_slots: Instant,
    /// The last `/slots` answer while the auto-stop counts down (compared with the next one).
    last_slots: Option<SlotsActivity>,
    next_refresh: Instant,
    next_log: Instant,
    next_status: Instant,
    /// Something changed: send a status snapshot at the end of this round.
    dirty: bool,
}

impl Service {
    fn new(config: AppConfig, out: Outbox) -> Self {
        let desired_install = config.bonsai_dir.as_deref().and_then(|dir| BonsaiInstall::detect(dir).ok());
        let built_url = config.base_url();
        let manager = ServerManager::new(desired_install.clone().unwrap_or_default(), &built_url);
        let (internal_tx, internal_rx) = mpsc::channel();
        let gpu_stop = Arc::new(AtomicBool::new(false));
        spawn_gpu_poller(internal_tx.clone(), Arc::clone(&gpu_stop));
        let install_ok = desired_install.as_ref().is_some_and(BonsaiInstall::is_usable);
        let now = Instant::now();
        Self {
            config,
            out,
            internal_tx,
            internal_rx,
            manager: Some(manager),
            built_install: desired_install.clone(),
            built_url,
            desired_install,
            install_ok,
            stopping: None,
            start_after_stop: false,
            queue: VecDeque::new(),
            running: None,
            start_wait: None,
            last_state: ServerState::Stopped,
            props: None,
            gpu: None,
            gpu_stop,
            log_tail: Vec::new(),
            log_stamp: None,
            log_read_at: None,
            tokens: None,
            slots_pending: false,
            next_slots: now,
            last_slots: None,
            next_refresh: now,
            next_log: now,
            next_status: now,
            dirty: true,
        }
    }

    /// The control loop: commands, helper messages, periodic work; ends after `Shutdown`.
    fn run(mut self, commands: &Receiver<Command>) {
        self.refresh_state();
        self.update_log_tail(true);
        self.send_status();
        loop {
            match commands.recv_timeout(TICK) {
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => self.handle_command(cmd),
                Err(RecvTimeoutError::Timeout) => {}
            }
            while let Ok(msg) = self.internal_rx.try_recv() {
                self.handle_internal(msg);
            }
            self.tick();
        }
        self.shutdown();
    }

    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Refresh => {
                self.refresh_state();
                self.update_log_tail(true);
                self.dirty = true;
            }
            Command::StartServer => self.manual_start(),
            Command::StopServer => self.manual_stop(),
            Command::RestartServer => self.restart(),
            Command::Submit { id, kind } => self.submit(id, kind),
            Command::Cancel(id) => self.cancel(id),
            Command::CountTokens { id, text } => self.count_tokens(id, text),
            Command::UpdateConfig(config) => {
                // A new server URL is applied by `reconcile_manager` on this round's tick.
                self.config = *config;
                self.dirty = true;
            }
            Command::SetInstall(install) => {
                self.install_ok = install.as_ref().is_some_and(BonsaiInstall::is_usable);
                self.desired_install = install;
                self.dirty = true;
            }
            Command::Shutdown => {} // handled by `run`
        }
    }

    fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::JobFinished(id) => {
                if self.running.as_ref().is_some_and(|job| job.id == id) {
                    self.running = None;
                }
                if let Some(mgr) = self.manager.as_mut() {
                    mgr.touch();
                }
                self.dirty = true;
            }
            Internal::StopDone { manager, result, reason } => self.stop_done(*manager, result, reason),
            Internal::Gpu(info) => {
                if self.gpu != info {
                    self.gpu = info;
                    self.dirty = true;
                }
            }
            Internal::Slots { result, last } => self.slots_checked(result, last),
        }
    }

    /// Periodic work, run after every command / helper message / timeout.
    fn tick(&mut self) {
        let now = Instant::now();
        if now >= self.next_refresh {
            self.refresh_state();
        }
        if now >= self.next_log {
            self.update_log_tail(false);
        }
        self.check_idle_stop();
        self.reconcile_manager();
        self.pump();
        if self.dirty || Instant::now() >= self.next_status {
            self.send_status();
        }
    }

    // --- status ---------------------------------------------------------------------------

    /// `ServerManager::refresh` and everything that follows from the new state.
    fn refresh_state(&mut self) {
        if let Some(mgr) = self.manager.as_mut() {
            mgr.refresh();
        }
        self.observe_state();
        let install_ok = self.desired_install.as_ref().is_some_and(BonsaiInstall::is_usable);
        if install_ok != self.install_ok {
            self.install_ok = install_ok;
            self.dirty = true;
        }
        let fast = self.start_wait.is_some() || self.last_state.is_busy();
        self.next_refresh = Instant::now() + if fast { REFRESH_FAST } else { REFRESH_EVERY };
    }

    /// React to state transitions: `/props` once per Ready transition, kept until it leaves Ready.
    fn observe_state(&mut self) {
        let Some(mgr) = self.manager.as_mut() else { return };
        let state = mgr.state().clone();
        if state != self.last_state {
            if state.is_ready() {
                // The idle countdown starts when the server is ready, not when it was launched.
                mgr.touch();
                self.props = mgr.client().props().ok();
            } else {
                self.props = None;
            }
            self.last_state = state;
            self.next_log = Instant::now(); // the log has news on every transition
            self.dirty = true;
        } else if state.is_ready() && self.props.is_none() {
            // The fetch at the transition failed: try again on each refresh.
            self.props = mgr.client().props().ok();
            if self.props.is_some() {
                self.dirty = true;
            }
        }
    }

    /// Re-read the log tail when the file changed (or `force`, or every 10 s).
    fn update_log_tail(&mut self, force: bool) {
        let now = Instant::now();
        self.next_log = now + LOG_EVERY;
        let Some(mgr) = self.manager.as_ref() else { return };
        let stamp = std::fs::metadata(mgr.log_path()).ok().map(|m| (m.len(), m.modified().ok()));
        let stale = match self.log_read_at {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= LOG_FORCE_EVERY,
        };
        if !force && !stale && stamp == self.log_stamp {
            return;
        }
        self.log_stamp = stamp;
        self.log_read_at = Some(now);
        let tail = if stamp.is_some() { mgr.log_tail(LOG_LINES) } else { Vec::new() };
        if tail != self.log_tail {
            self.log_tail = tail;
            self.dirty = true;
        }
    }

    fn jobs_active(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }

    fn snapshot(&self) -> StatusSnapshot {
        let mut snap = StatusSnapshot {
            install_ok: self.install_ok,
            install: self.desired_install.clone(),
            gpu: self.gpu.clone(),
            busy: self.running.is_some(),
            queued: self.queue.len(),
            log_tail: self.log_tail.clone(),
            ..StatusSnapshot::default()
        };
        match (&self.manager, &self.stopping) {
            (Some(mgr), None) => {
                let idle = mgr.idle_for();
                snap.state = mgr.state().clone();
                snap.started_by_us = mgr.started_by_us();
                snap.props = if snap.state.is_ready() { self.props.clone() } else { None };
                snap.idle_secs = idle.as_secs();
                snap.auto_stop_in_secs = auto_stop_in(
                    snap.started_by_us,
                    &snap.state,
                    self.jobs_active(),
                    self.config.auto_stop_idle_minutes,
                    idle,
                )
                .map(|left| left.as_secs());
                snap.uptime_secs = mgr.uptime().map(|d| d.as_secs());
            }
            (_, stopping) => {
                snap.state = ServerState::Stopping;
                snap.started_by_us = stopping.as_ref().is_some_and(|s| s.started_by_us);
            }
        }
        snap
    }

    fn send_status(&mut self) {
        let snap = self.snapshot();
        self.out.send(Event::Status(Box::new(snap)));
        self.dirty = false;
        self.next_status = Instant::now() + STATUS_EVERY;
    }

    // --- install / URL changes ------------------------------------------------------------

    /// Rebuild the manager when the install or the server URL changed - unless that would lose
    /// track of something: a running job, jobs waiting for a start, a stop in progress, or a
    /// server this app started that is still up (idle auto-stop and stop-on-exit need the
    /// manager that started it). Returns true when it rebuilt.
    fn reconcile_manager(&mut self) -> bool {
        let want_url = self.config.base_url();
        if self.built_install == self.desired_install && self.built_url == want_url {
            return false;
        }
        if self.running.is_some() || self.start_wait.is_some() {
            return false;
        }
        let Some(mgr) = self.manager.as_ref() else { return false };
        if mgr.started_by_us() && !matches!(mgr.state(), ServerState::Stopped | ServerState::Failed(_)) {
            return false;
        }
        let install = self.desired_install.clone().unwrap_or_default();
        self.manager = Some(ServerManager::new(install, &want_url));
        self.built_install = self.desired_install.clone();
        self.built_url = want_url;
        self.props = None;
        self.log_stamp = None;
        self.refresh_state();
        self.update_log_tail(true);
        self.dirty = true;
        true
    }

    fn no_install_error(&self) -> String {
        let problems = self.desired_install.as_ref().map(BonsaiInstall::problems).unwrap_or_default();
        no_install_message(&problems)
    }

    // --- jobs -----------------------------------------------------------------------------

    fn submit(&mut self, id: JobId, kind: JobKind) {
        self.out.send(Event::JobQueued(id));
        self.queue.push_back(QueuedJob { id, kind, phase: None });
        // Decide on a fresh state: the server may have gone away since the last refresh.
        if self.running.is_none() && self.start_wait.is_none() && self.manager.is_some() {
            self.refresh_state();
        }
        self.dirty = true;
    }

    fn cancel(&mut self, id: JobId) {
        let before = self.queue.len();
        self.queue.retain(|job| job.id != id);
        if self.queue.len() != before {
            self.out.send(Event::JobCancelled(id));
            self.dirty = true;
        }
        if self.running.as_ref().is_some_and(|job| job.id == id) {
            self.cancel_running();
        }
    }

    /// Ask the running job to stop; its thread then sends `JobCancelled`.
    fn cancel_running(&self) {
        if let Some(job) = &self.running {
            if !job.cancel.swap(true, Ordering::SeqCst) {
                self.out.send(Event::JobPhase { id: job.id, phase: phase::CANCELLING.to_string() });
            }
        }
    }

    /// Cancel every queued job (`JobCancelled`) and the running one.
    fn cancel_all(&mut self) {
        let ids: Vec<JobId> = self.queue.drain(..).map(|job| job.id).collect();
        for id in ids {
            self.out.send(Event::JobCancelled(id));
        }
        self.start_wait = None;
        self.cancel_running();
        self.dirty = true;
    }

    fn fail_queue(&mut self, error: &str) {
        for job in std::mem::take(&mut self.queue) {
            self.out.send(Event::JobFailed { id: job.id, error: error.to_string() });
        }
        self.start_wait = None;
        self.dirty = true;
    }

    fn set_queue_phase(&mut self, text: &'static str) {
        for job in &mut self.queue {
            if job.phase != Some(text) {
                job.phase = Some(text);
                self.out.send(Event::JobPhase { id: job.id, phase: text.to_string() });
            }
        }
    }

    /// Move the queue forward: dispatch, wait for the server, start it, or fail the jobs.
    fn pump(&mut self) {
        if self.queue.is_empty() {
            self.start_wait = None;
            return;
        }
        if self.running.is_some() {
            self.set_queue_phase(phase::QUEUED);
            return;
        }
        if self.manager.is_none() {
            self.set_queue_phase(phase::WAIT_STOP);
            return;
        }
        let now = Instant::now();
        let state = self.last_state.clone();
        match queue_action(&state, self.start_wait, now, start_timeout(&self.config)) {
            QueueAction::Dispatch => {
                self.start_wait = None;
                self.dispatch();
            }
            QueueAction::Wait(text) => {
                if self.start_wait.is_none() {
                    self.start_wait = Some(StartWait { since: now, issued: false });
                }
                self.set_queue_phase(text);
            }
            QueueAction::Fail(error) => self.fail_queue(&error),
            QueueAction::NeedStart => self.start_for_queue(),
        }
    }

    /// The server is down and jobs wait: fail them, or auto-start (non-forced) without blocking.
    fn start_for_queue(&mut self) {
        self.start_wait = None;
        if self.reconcile_manager() {
            return; // new install / URL: decide on the next round with the new manager's state
        }
        let usable = self.manager.as_ref().is_some_and(|m| m.install().is_usable());
        match start_decision(usable, self.config.auto_start) {
            StartDecision::NoInstall => {
                let error = self.no_install_error();
                self.fail_queue(&error);
            }
            StartDecision::AutoStartOff => self.fail_queue(NOT_RUNNING),
            StartDecision::Start => match self.launch(false) {
                Ok(()) => {
                    self.start_wait = Some(StartWait { since: Instant::now(), issued: true });
                    self.next_refresh = Instant::now() + REFRESH_FAST;
                    self.set_queue_phase(phase::STARTING);
                }
                // A server came up meanwhile (e.g. start_bonsai.bat): the next round uses or waits for it.
                Err(_)
                    if matches!(self.last_state, ServerState::Ready | ServerState::Loading | ServerState::Starting) => {
                }
                Err(error) => self.fail_queue(&error),
            },
        }
    }

    /// Run the next job on a worker thread.
    fn dispatch(&mut self) {
        let Some(job) = self.queue.pop_front() else { return };
        let Some(mgr) = self.manager.as_mut() else {
            self.queue.push_front(job);
            return;
        };
        mgr.touch();
        let id = job.id;
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = JobContext {
            id,
            kind: job.kind,
            client: mgr.client().clone(),
            cancel: Arc::clone(&cancel),
            out: self.out.clone(),
        };
        let done = self.internal_tx.clone();
        match thread::Builder::new().name(format!("bh3-job-{id}")).spawn(move || job_thread(ctx, done)) {
            Ok(_) => self.running = Some(RunningJob { id, cancel }),
            Err(e) => self.out.send(Event::JobFailed { id, error: format!("Could not start the job thread: {e}") }),
        }
        self.dirty = true;
    }

    fn count_tokens(&mut self, id: JobId, text: String) {
        let Some(mgr) = self.manager.as_ref() else { return };
        if !mgr.state().is_ready() {
            return; // needs a Ready server; the UI sees the state and asks again later
        }
        let mut request = TokenRequest { id, text, client: mgr.client().clone() };
        if let Some(tx) = &self.tokens {
            match tx.send(request) {
                Ok(()) => return,
                Err(mpsc::SendError(back)) => request = back, // thread gone: start a new one
            }
        }
        let (tx, rx) = mpsc::channel();
        let out = self.out.clone();
        match thread::Builder::new().name("bh3-tokens".into()).spawn(move || token_thread(rx, out)) {
            Ok(_) => {
                let _ = tx.send(request);
                self.tokens = Some(tx);
            }
            Err(e) => self.out.error(format!("Token count failed: {e}")),
        }
    }

    // --- server start / stop --------------------------------------------------------------

    /// Spawn the launcher (non-blocking). The error is ready to show.
    fn launch(&mut self, force: bool) -> Result<(), String> {
        let Some(usable) = self.manager.as_ref().map(|m| m.install().is_usable()) else {
            return Err("Bonsai is still stopping - try again in a moment".to_string());
        };
        if !usable {
            return Err(self.no_install_error());
        }
        let opts = StartOptions { force, ..StartOptions::from_config(&self.config) };
        let started = match self.manager.as_mut() {
            Some(mgr) => mgr.start(&opts).map(|()| mgr.touch()),
            None => return Err("Bonsai is still stopping - try again in a moment".to_string()),
        };
        if let Err(e) = started {
            // `start` may have found a server that already answers (and set the state to it).
            self.observe_state();
            return Err(format!("Could not start Bonsai: {e:#}"));
        }
        self.refresh_state();
        self.next_log = Instant::now();
        Ok(())
    }

    /// Start button: forced (other programs' VRAM use is not checked).
    fn manual_start(&mut self) {
        if self.stopping.is_some() {
            self.start_after_stop = true;
            self.out.notice("Bonsai is stopping - it starts again right after");
            return;
        }
        self.reconcile_manager();
        self.refresh_state();
        match self.last_state.clone() {
            ServerState::Ready => self.out.notice("Bonsai is already running"),
            ServerState::Starting | ServerState::Loading => self.out.notice("Bonsai is already starting"),
            _ => {
                if let Err(error) = self.launch(true) {
                    self.out.error(error);
                }
            }
        }
        self.dirty = true;
    }

    /// Stop button: cancels every job, then stops on the helper thread.
    fn manual_stop(&mut self) {
        self.start_after_stop = false;
        self.cancel_all();
        if self.stopping.is_some() {
            return; // already stopping
        }
        self.refresh_state();
        if self.last_state == ServerState::Stopped && !self.server_process_runs() {
            self.out.notice("Bonsai is not running");
            return;
        }
        self.begin_stop(StopReason::Manual);
    }

    /// A llama-server on the server URL's port or from the install folder runs (one that does not
    /// answer yet); llama-server processes of other programs do not count.
    fn server_process_runs(&self) -> bool {
        self.manager.as_ref().is_some_and(|mgr| !mgr.server_pids().is_empty())
    }

    /// Restart: cancel the running job, stop, then start (forced); queued jobs wait for it.
    /// Without a usable install nothing is stopped: Bonsai could not be started again.
    fn restart(&mut self) {
        if !self.install_ok {
            self.out.error(format!("Bonsai was not restarted (it could not start again): {}", self.no_install_error()));
            return;
        }
        self.cancel_running();
        if self.stopping.is_some() {
            self.start_after_stop = true;
            return;
        }
        self.refresh_state();
        if self.last_state == ServerState::Stopped && !self.server_process_runs() {
            self.manual_start();
            return;
        }
        self.start_after_stop = true;
        self.begin_stop(StopReason::Restart);
    }

    /// Run `ServerManager::stop` (up to ~10 s) on a helper thread that owns the manager until it
    /// hands it back with `Internal::StopDone`.
    fn begin_stop(&mut self, reason: StopReason) {
        let Some(mut mgr) = self.manager.take() else { return };
        let started_by_us = mgr.started_by_us();
        let tx = self.internal_tx.clone();
        let spawned = thread::Builder::new().name("bh3-stop".into()).spawn(move || {
            let result = match panic::catch_unwind(AssertUnwindSafe(|| mgr.stop())) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(payload) => Err(format!("internal error: {}", panic_message(payload))),
            };
            let _ = tx.send(Internal::StopDone { manager: Box::new(mgr), result, reason });
        });
        match spawned {
            Ok(_) => self.stopping = Some(StopInProgress { started_by_us }),
            Err(e) => {
                // The manager went down with the closure: carry on with a fresh one.
                self.out.error(format!("Could not stop Bonsai: {e}"));
                let install = self.built_install.clone().unwrap_or_default();
                self.manager = Some(ServerManager::new(install, &self.built_url));
                self.refresh_state();
            }
        }
        self.dirty = true;
    }

    fn stop_done(&mut self, manager: ServerManager, result: Result<(), String>, reason: StopReason) {
        self.manager = Some(manager);
        self.stopping = None;
        self.refresh_state();
        self.update_log_tail(true);
        let stopped = result.is_ok();
        match result {
            Ok(()) => match reason {
                StopReason::Manual => self.out.notice("Bonsai stopped"),
                StopReason::Idle(minutes) => self.out.notice(idle_stop_notice(minutes)),
                StopReason::Restart => {}
            },
            Err(e) => {
                self.out.error(format!("Could not stop Bonsai: {e}"));
                if let (StopReason::Idle(_), Some(mgr)) = (reason, self.manager.as_mut()) {
                    mgr.touch(); // try again after another idle period, not on every round
                }
            }
        }
        if std::mem::take(&mut self.start_after_stop) && stopped {
            self.manual_start();
        }
        self.dirty = true;
    }

    /// Time left until the idle auto-stop, when it applies now.
    fn auto_stop_left(&self) -> Option<Duration> {
        let mgr = self.manager.as_ref()?;
        let minutes = self.config.auto_stop_idle_minutes;
        auto_stop_in(mgr.started_by_us(), mgr.state(), self.jobs_active(), minutes, mgr.idle_for())
    }

    /// While the idle auto-stop counts down: a `/slots` check every [`SLOTS_EVERY`], and one more
    /// when the stop is due; the stop itself happens when that answer comes back quiet.
    fn check_idle_stop(&mut self) {
        if self.stopping.is_some() || self.slots_pending {
            return;
        }
        match self.auto_stop_left() {
            None => self.last_slots = None, // a new countdown compares from scratch
            Some(left) if left.is_zero() => self.check_slots(true),
            Some(_) if Instant::now() >= self.next_slots => self.check_slots(false),
            Some(_) => {}
        }
    }

    /// Ask `/slots` on a helper thread (it can take seconds while a long prompt is processed);
    /// the answer comes back as `Internal::Slots`.
    fn check_slots(&mut self, last: bool) {
        let Some(mgr) = self.manager.as_ref() else { return };
        let client = mgr.client().clone();
        let tx = self.internal_tx.clone();
        self.next_slots = Instant::now() + SLOTS_EVERY;
        let spawned = thread::Builder::new().name("bh3-slots".into()).spawn(move || {
            let result = match panic::catch_unwind(AssertUnwindSafe(|| client.slots_activity(SLOTS_TIMEOUT))) {
                Ok(Ok(activity)) => Ok(activity),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(payload) => Err(format!("internal error: {}", panic_message(payload))),
            };
            let _ = tx.send(Internal::Slots { result, last });
        });
        match spawned {
            Ok(_) => self.slots_pending = true,
            Err(e) => self.slots_checked(Err(e.to_string()), last),
        }
    }

    /// A `/slots` answer: other programs' activity resets the idle timer; a quiet answer to the
    /// last check stops the server; no answer to it postpones the stop by another idle period.
    fn slots_checked(&mut self, result: Result<Option<SlotsActivity>, String>, last: bool) {
        self.slots_pending = false;
        // A job, a stop or a state change may have come in meanwhile.
        let Some(left) = self.auto_stop_left() else { return };
        let busy = slots_say_busy(&result, self.last_slots.as_ref(), last);
        if let Ok(Some(activity)) = result {
            self.last_slots = Some(activity);
        }
        if busy {
            if let Some(mgr) = self.manager.as_mut() {
                mgr.touch();
            }
            self.dirty = true;
        } else if last && left.is_zero() {
            self.begin_stop(StopReason::Idle(self.config.auto_stop_idle_minutes));
        }
    }

    // --- shutdown -------------------------------------------------------------------------

    fn shutdown(&mut self) {
        let ids: Vec<JobId> = self.queue.drain(..).map(|job| job.id).collect();
        for id in ids {
            self.out.send(Event::JobCancelled(id));
        }
        self.start_wait = None;
        self.start_after_stop = false;
        self.cancel_running();

        // Let the cancelled job say goodbye and a stop in progress hand the manager back.
        let begun = Instant::now();
        loop {
            let waited = begun.elapsed();
            let wait_job = self.running.is_some() && waited < SHUTDOWN_JOB_WAIT;
            let wait_stop = self.manager.is_none() && waited < SHUTDOWN_STOP_WAIT;
            if !wait_job && !wait_stop {
                break;
            }
            match self.internal_rx.recv_timeout(Duration::from_millis(50)) {
                // A late `/slots` answer must not begin an idle stop now.
                Ok(Internal::Slots { .. }) => {}
                Ok(msg) => self.handle_internal(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        if self.config.stop_on_exit {
            if let Some(mgr) = self.manager.as_mut() {
                mgr.refresh();
                if mgr.started_by_us() {
                    if let Err(e) = mgr.stop() {
                        self.out.error(format!("Could not stop Bonsai: {e:#}"));
                    }
                }
            }
        }
        self.gpu_stop.store(true, Ordering::Relaxed);
        self.out.send(Event::ShutdownComplete);
    }
}

// ---------------------------------------------------------------------------------------------
// Worker threads

/// Everything a job thread needs.
struct JobContext {
    id: JobId,
    kind: JobKind,
    client: BonsaiClient,
    cancel: Arc<AtomicBool>,
    out: Outbox,
}

/// Job thread body: run the job, send its final event, tell the control thread.
fn job_thread(ctx: JobContext, done: Sender<Internal>) {
    let id = ctx.id;
    let terminal = match panic::catch_unwind(AssertUnwindSafe(|| run_job(&ctx))) {
        Ok(event) => event,
        Err(payload) => Event::JobFailed { id, error: format!("internal error: {}", panic_message(payload)) },
    };
    ctx.out.send(terminal);
    let _ = done.send(Internal::JobFinished(id));
}

/// Run one job, sending phases and deltas on the way; returns the final event.
fn run_job(ctx: &JobContext) -> Event {
    let id = ctx.id;
    let out = &ctx.out;
    out.send(Event::JobPhase { id, phase: phase::PROMPT.to_string() });
    let (messages, params): (Cow<'_, [ChatMessage]>, &GenParams) = match &ctx.kind {
        // Loaded per job so Library edits apply to the next prompt.
        JobKind::Write(job) => (Cow::Owned(writer::build_messages(&Library::load(), job)), &job.params),
        JobKind::Chat { messages, params } => (Cow::Borrowed(messages.as_slice()), params),
    };

    let mut batcher = DeltaBatcher::new(DELTA_EVERY);
    let mut current: Option<StreamPhase> = None;
    let send_delta = |(reasoning, content): (String, String)| out.send(Event::JobDelta { id, reasoning, content });
    let streamed = ctx.client.chat_stream(&messages, params, &ctx.cancel, &mut |piece| {
        let now = Instant::now();
        match next_phase(current, &piece) {
            Some(next) => {
                // Old phase's text first, then the phase, then the new piece without delay.
                if let Some(delta) = batcher.take(now) {
                    send_delta(delta);
                }
                current = Some(next);
                out.send(Event::JobPhase { id, phase: next.label().to_string() });
                batcher.push(piece);
                if let Some(delta) = batcher.take(now) {
                    send_delta(delta);
                }
            }
            None => {
                batcher.push(piece);
                if let Some(delta) = batcher.take_if_due(now) {
                    send_delta(delta);
                }
            }
        }
    });
    if let Some(delta) = batcher.take(Instant::now()) {
        send_delta(delta);
    }

    let cancelled = ctx.cancel.load(Ordering::SeqCst);
    match streamed {
        Err(_) if cancelled => Event::JobCancelled(id),
        Err(e) => Event::JobFailed { id, error: format!("Request to Bonsai failed: {e:#}") },
        Ok(result) if result.cancelled => Event::JobCancelled(id),
        Ok(result) => finish_job(id, &ctx.kind, result),
    }
}

/// Final event of a completed request.
fn finish_job(id: JobId, kind: &JobKind, result: ChatResult) -> Event {
    match kind {
        JobKind::Write(job) => {
            // Checked before `finish` so an empty answer is not saved to history.
            if writer::clean_output(&result.content).trim().is_empty() {
                Event::JobFailed { id, error: empty_answer_message(&result.finish_reason, &job.params) }
            } else {
                Event::WriteDone { id, outcome: Box::new(writer::finish(job, result)) }
            }
        }
        JobKind::Chat { .. } => Event::ChatDone { id, result: Box::new(result) },
    }
}

/// Answers `/tokenize` requests in order until the control thread drops its sender.
fn token_thread(requests: Receiver<TokenRequest>, out: Outbox) {
    for req in requests {
        let counted = panic::catch_unwind(AssertUnwindSafe(|| req.client.tokenize_count(&req.text)));
        match counted {
            Ok(Ok(count)) => out.send(Event::TokenCount { id: req.id, count }),
            Ok(Err(e)) => out.error(format!("Token count failed: {e:#}")),
            Err(payload) => out.error(format!("Token count failed: internal error: {}", panic_message(payload))),
        }
    }
}

/// Poll the GPU on a small thread (nvidia-smi takes 100-300 ms) until `stop` is set or the
/// control thread is gone. Without the thread the snapshot simply has no GPU info.
fn spawn_gpu_poller(tx: Sender<Internal>, stop: Arc<AtomicBool>) {
    let _ = thread::Builder::new().name("bh3-gpu".into()).spawn(move || {
        let mut misses = 0u32;
        while !stop.load(Ordering::Relaxed) {
            let Ok(info) = panic::catch_unwind(gpu::query) else { return };
            misses = if info.is_some() { 0 } else { misses.saturating_add(1) };
            if tx.send(Internal::Gpu(info)).is_err() {
                return;
            }
            sleep_unless_stopped(&stop, gpu_poll_interval(misses));
        }
    });
}

/// Sleep `total` in small steps, returning early once `stop` is set.
fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let end = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::client::Health;
    use crate::gen::Mode;

    // ---- pure decision logic ------------------------------------------------------------

    #[test]
    fn batcher_sends_first_piece_at_once_then_batches() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut b = DeltaBatcher::new(ms(50));
        assert_eq!(b.take_if_due(t0), None, "nothing pending");
        b.push(StreamEvent::Content("Hel".into()));
        assert_eq!(b.take_if_due(t0), Some((String::new(), "Hel".into())));
        b.push(StreamEvent::Content("lo".into()));
        b.push(StreamEvent::Reasoning("hmm".into()));
        assert_eq!(b.take_if_due(t0 + ms(20)), None, "inside the interval");
        b.push(StreamEvent::Content(" world".into()));
        assert_eq!(b.take_if_due(t0 + ms(50)), Some(("hmm".into(), "lo world".into())));
        assert!(b.is_empty());
        b.push(StreamEvent::Reasoning("x".into()));
        assert_eq!(b.take_if_due(t0 + ms(60)), None);
        assert_eq!(b.take(t0 + ms(61)), Some(("x".into(), String::new())), "forced flush");
        assert_eq!(b.take(t0 + ms(62)), None);
    }

    #[test]
    fn phases_move_forward_once() {
        let think = StreamEvent::Reasoning("let me see".into());
        let write = StreamEvent::Content("subject_definitions:".into());
        let blank = StreamEvent::Content("\n\n".into());
        assert_eq!(next_phase(None, &blank), None, "whitespace does not start writing");
        assert_eq!(next_phase(None, &StreamEvent::Reasoning(String::new())), None);
        assert_eq!(next_phase(None, &think), Some(StreamPhase::Thinking));
        assert_eq!(next_phase(Some(StreamPhase::Thinking), &think), None);
        assert_eq!(next_phase(Some(StreamPhase::Thinking), &blank), None);
        assert_eq!(next_phase(Some(StreamPhase::Thinking), &write), Some(StreamPhase::Writing));
        assert_eq!(next_phase(Some(StreamPhase::Writing), &think), None, "never back to thinking");
        assert_eq!(next_phase(None, &write), Some(StreamPhase::Writing), "thinking off");
        assert_eq!(StreamPhase::Thinking.label(), "Thinking...");
        assert_eq!(StreamPhase::Writing.label(), "Writing...");
    }

    #[test]
    fn auto_stop_only_for_our_idle_ready_server() {
        let secs = Duration::from_secs;
        let ready = ServerState::Ready;
        let should_auto_stop = |ours, state: &ServerState, jobs, minutes, idle| {
            auto_stop_in(ours, state, jobs, minutes, idle).is_some_and(|left| left.is_zero())
        };
        assert!(should_auto_stop(true, &ready, false, 10, secs(600)));
        assert!(should_auto_stop(true, &ready, false, 10, secs(605)));
        assert!(!should_auto_stop(true, &ready, false, 10, secs(599)));
        assert!(!should_auto_stop(false, &ready, false, 10, secs(9999)), "started elsewhere");
        assert!(!should_auto_stop(true, &ready, true, 10, secs(9999)), "job running or waiting");
        assert!(!should_auto_stop(true, &ready, false, 0, secs(9999)), "auto-stop off");
        assert!(!should_auto_stop(true, &ServerState::Loading, false, 10, secs(9999)), "still loading");
        assert_eq!(auto_stop_in(true, &ready, false, 10, secs(90)), Some(secs(510)));
        assert_eq!(auto_stop_in(true, &ready, false, 1, secs(90)), Some(Duration::ZERO));
        assert_eq!(auto_stop_in(false, &ready, false, 10, Duration::ZERO), None);
        assert_eq!(auto_stop_in(true, &ServerState::Stopped, false, 10, Duration::ZERO), None);
    }

    #[test]
    fn other_clients_activity_postpones_the_idle_stop() {
        let quiet = SlotsActivity { processing: false, tasks: vec![(0, 100, 0), (1, 90, 0)] };
        let working = SlotsActivity { processing: true, ..quiet.clone() };
        let ran = SlotsActivity { processing: false, tasks: vec![(0, 100, 0), (1, 120, 0)] };
        assert!(!slots_say_busy(&Ok(Some(quiet.clone())), None, true));
        assert!(!slots_say_busy(&Ok(Some(quiet.clone())), Some(&quiet), true));
        assert!(slots_say_busy(&Ok(Some(working)), Some(&quiet), false), "a slot is processing");
        assert!(slots_say_busy(&Ok(Some(ran)), Some(&quiet), true), "a request ran in between");
        assert!(!slots_say_busy(&Ok(None), None, true), "no slots endpoint: stop as before");
        assert!(slots_say_busy(&Err("timed out".into()), None, true), "no answer before the stop: wait");
        assert!(!slots_say_busy(&Err("timed out".into()), None, false));
    }

    #[test]
    fn start_decision_checks_install_before_auto_start() {
        assert_eq!(start_decision(true, true), StartDecision::Start);
        assert_eq!(start_decision(true, false), StartDecision::AutoStartOff);
        assert_eq!(start_decision(false, true), StartDecision::NoInstall);
        assert_eq!(start_decision(false, false), StartDecision::NoInstall);
    }

    #[test]
    fn queue_action_covers_server_states() {
        let secs = Duration::from_secs;
        let t0 = Instant::now();
        let limit = secs(300);
        let ours = Some(StartWait { since: t0, issued: true });
        let theirs = Some(StartWait { since: t0, issued: false });
        let failed = ServerState::Failed("CUDA out of memory".into());

        assert_eq!(queue_action(&ServerState::Ready, ours, t0, limit), QueueAction::Dispatch);
        assert_eq!(queue_action(&ServerState::Starting, None, t0, limit), QueueAction::Wait(phase::STARTING));
        assert_eq!(queue_action(&ServerState::Loading, ours, t0 + secs(30), limit), QueueAction::Wait(phase::LOADING));
        assert_eq!(queue_action(&ServerState::Stopping, None, t0, limit), QueueAction::Wait(phase::WAIT_STOP));
        match queue_action(&ServerState::Loading, theirs, t0 + secs(300), limit) {
            QueueAction::Fail(msg) => assert!(msg.contains("within 300 s") && msg.contains("Loading"), "{msg}"),
            other => panic!("expected a timeout, got {other:?}"),
        }
        let reason = QueueAction::Fail("Bonsai failed to start: CUDA out of memory".into());
        assert_eq!(queue_action(&failed, ours, t0, limit), reason);
        assert_eq!(queue_action(&failed, theirs, t0, limit), reason, "a start begun elsewhere failed");
        assert_eq!(queue_action(&failed, None, t0, limit), QueueAction::NeedStart, "old failure: try again");
        assert_eq!(queue_action(&ServerState::Stopped, None, t0, limit), QueueAction::NeedStart);
        assert_eq!(queue_action(&ServerState::Stopped, theirs, t0, limit), QueueAction::NeedStart);
        assert_eq!(
            queue_action(&ServerState::Stopped, ours, t0 + secs(1), limit),
            QueueAction::Wait(phase::STARTING),
            "launcher not visible yet"
        );
        match queue_action(&ServerState::Stopped, ours, t0 + secs(6), limit) {
            QueueAction::Fail(msg) => assert!(msg.contains("stopped while starting"), "{msg}"),
            other => panic!("expected a failed start, got {other:?}"),
        }
    }

    #[test]
    fn waiting_phases_per_state() {
        assert_eq!(waiting_phase(&ServerState::Starting), Some("Starting Bonsai..."));
        assert_eq!(waiting_phase(&ServerState::Loading), Some("Loading model..."));
        assert_eq!(waiting_phase(&ServerState::Stopping), Some(phase::WAIT_STOP));
        assert_eq!(waiting_phase(&ServerState::Ready), None);
        assert_eq!(waiting_phase(&ServerState::Stopped), None);
    }

    #[test]
    fn job_error_messages() {
        let off = GenParams::prompt_writer();
        let high = GenParams { thinking: ThinkingMode::High, max_tokens: 1500, ..GenParams::prompt_writer() };
        assert_eq!(empty_answer_message("stop", &off), "Bonsai returned an empty answer (finish_reason stop)");
        assert_eq!(empty_answer_message(" ", &off), "Bonsai returned an empty answer (finish_reason none)");
        let thinking = empty_answer_message("length", &high);
        assert!(thinking.starts_with("Bonsai returned an empty answer (finish_reason length)"));
        assert!(thinking.contains("turn thinking off or raise max tokens"), "{thinking}");
        assert!(thinking.contains("1500"), "{thinking}");
        let plain = empty_answer_message("length", &off);
        assert!(plain.contains("raise max tokens") && !plain.contains("thinking"), "{plain}");

        assert_eq!(idle_stop_notice(10), "Bonsai stopped after 10 idle minutes - GPU free for ComfyUI");
        assert_eq!(idle_stop_notice(1), "Bonsai stopped after 1 idle minute - GPU free for ComfyUI");

        assert_eq!(no_install_message(&[]), "Choose the Bonsai folder first (Bonsai workspace)");
        let listed = no_install_message(&["no launcher script".into(), "no model".into()]);
        assert_eq!(listed, "Choose the Bonsai folder first (Bonsai workspace): no launcher script; no model");

        assert_eq!(start_failed_message(&ServerState::Failed(" boom ".into())), "Bonsai failed to start: boom");
        assert!(start_failed_message(&ServerState::Stopped).contains("see the log"));
        assert_eq!(NOT_RUNNING, "Bonsai is not running - press Start or enable auto-start");
    }

    #[test]
    fn start_timeout_and_gpu_interval() {
        let cfg = |secs| AppConfig { start_timeout_secs: secs, ..AppConfig::default() };
        assert_eq!(start_timeout(&cfg(0)), Duration::from_secs(300));
        assert_eq!(start_timeout(&cfg(120)), Duration::from_secs(120));
        assert_eq!(gpu_poll_interval(0), GPU_EVERY);
        assert_eq!(gpu_poll_interval(2), GPU_EVERY);
        assert_eq!(gpu_poll_interval(3), GPU_EVERY_MISSING);
    }

    #[test]
    fn panic_messages_are_readable() {
        assert_eq!(panic_message(Box::new("static text")), "static text");
        assert_eq!(panic_message(Box::new(String::from("owned text"))), "owned text");
        assert_eq!(panic_message(Box::new(42_u8)), "unknown panic");
    }

    // ---- the service thread (no live server needed) ---------------------------------------

    /// Nothing listens here, so these tests never touch the user's server on 8080.
    const DEAD_URL: &str = "http://127.0.0.1:9";

    /// Records every event received while waiting, for assertions afterwards.
    struct Events<'a> {
        svc: &'a ServiceHandle,
        seen: Vec<Event>,
    }

    impl<'a> Events<'a> {
        fn new(svc: &'a ServiceHandle) -> Self {
            Self { svc, seen: Vec::new() }
        }

        /// Next event matching `pred` within `timeout`.
        fn wait_for(&mut self, timeout: Duration, mut pred: impl FnMut(&Event) -> bool) -> Option<Event> {
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                match self.svc.rx.recv_timeout(left) {
                    Ok(ev) => {
                        let hit = pred(&ev);
                        self.seen.push(ev.clone());
                        if hit {
                            return Some(ev);
                        }
                    }
                    Err(_) => return None,
                }
            }
        }

        fn saw(&self, pred: impl Fn(&Event) -> bool) -> bool {
            self.seen.iter().any(pred)
        }

        fn last_status(&self) -> Option<&StatusSnapshot> {
            self.seen.iter().rev().find_map(|ev| match ev {
                Event::Status(s) => Some(&**s),
                _ => None,
            })
        }

        /// Job and toast events (no status spam) for failure messages.
        fn job_events(&self) -> Vec<&Event> {
            self.seen.iter().filter(|ev| !matches!(ev, Event::Status(_))).collect()
        }
    }

    fn chat_job(text: &str, max_tokens: u32) -> JobKind {
        JobKind::Chat {
            messages: vec![ChatMessage::user(text)],
            params: GenParams { thinking: ThinkingMode::Off, temperature: 0.2, max_tokens, ..GenParams::chat() },
        }
    }

    /// Empty files in the Bonsai-demo layout (never launched: these tests keep auto-start off).
    fn fake_install(dir: &Path) -> BonsaiInstall {
        let file = |rel: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
            std::fs::write(&path, b"").expect("write file");
            path
        };
        BonsaiInstall {
            root: dir.to_path_buf(),
            launcher_ps1: Some(file("scripts/start_llama_server.ps1")),
            launcher_bat: None,
            server_exe: Some(file("bin/cuda/llama-server.exe")),
            models: vec![file("models/bonsai2-gguf/27B/Fake-Bonsai-2-27B-PTQ1_0.gguf")],
            mmproj: Some(file("models/bonsai2-gguf/27B/Fake-Bonsai-2-27B-mmproj-Q8_0.gguf")),
        }
    }

    #[test]
    fn job_fails_fast_without_install() {
        let cfg = AppConfig { bonsai_dir: None, server_url: DEAD_URL.into(), auto_start: true, ..AppConfig::default() };
        let svc = spawn(cfg, Arc::new(|| {}));
        let mut ev = Events::new(&svc);
        svc.send(Command::CountTokens { id: 5, text: "ignored: the server is not ready".into() });
        svc.send(Command::Submit { id: 1, kind: chat_job("hi", 8) });
        let failed = ev.wait_for(Duration::from_secs(15), |e| matches!(e, Event::JobFailed { id: 1, .. }));
        assert_eq!(failed, Some(Event::JobFailed { id: 1, error: NO_INSTALL.to_string() }), "{:#?}", ev.job_events());
        assert!(ev.saw(|e| *e == Event::JobQueued(1)));
        let idle = ev.wait_for(Duration::from_secs(5), |e| matches!(e, Event::Status(s) if s.queued == 0));
        let Some(Event::Status(status)) = idle else { panic!("no status after the failure") };
        assert!(!status.install_ok && status.install.is_none());
        assert_eq!(status.state, ServerState::Stopped);
        assert!(!status.busy && !status.started_by_us && status.props.is_none());
        assert_eq!(status.auto_stop_in_secs, None);

        svc.send(Command::Shutdown);
        assert!(ev.wait_for(Duration::from_secs(15), |e| *e == Event::ShutdownComplete).is_some());
        assert!(!ev.saw(|e| matches!(e, Event::TokenCount { .. })), "no token count while not ready");
    }

    #[test]
    fn job_fails_when_server_is_down_and_auto_start_is_off() {
        let dir = std::env::temp_dir().join(format!("bh3-service-test-{}", std::process::id()));
        let install = fake_install(&dir);
        let cfg =
            AppConfig { bonsai_dir: None, server_url: DEAD_URL.into(), auto_start: false, ..AppConfig::default() };
        let svc = spawn(cfg, Arc::new(|| {}));
        let mut ev = Events::new(&svc);
        svc.send(Command::SetInstall(Some(install.clone())));
        svc.send(Command::Cancel(42)); // unknown id: ignored
        svc.send(Command::Submit { id: 2, kind: chat_job("hi", 8) });
        let failed = ev.wait_for(Duration::from_secs(15), |e| matches!(e, Event::JobFailed { id: 2, .. }));
        assert_eq!(failed, Some(Event::JobFailed { id: 2, error: NOT_RUNNING.to_string() }), "{:#?}", ev.job_events());
        let status = ev.wait_for(Duration::from_secs(5), |e| matches!(e, Event::Status(s) if s.queued == 0));
        let Some(Event::Status(status)) = status else { panic!("no status after the failure") };
        assert_eq!(status.install.as_ref(), Some(&install));
        assert!(status.install_ok, "fake install should be usable: {:?}", install);
        assert!(!status.started_by_us);
        assert!(!ev.saw(|e| matches!(e, Event::JobCancelled(_))));

        svc.send(Command::Shutdown);
        assert!(svc.wait_shutdown(Duration::from_secs(15)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_without_install_stops_nothing() {
        let cfg = AppConfig { bonsai_dir: None, server_url: DEAD_URL.into(), auto_start: true, ..AppConfig::default() };
        let svc = spawn(cfg, Arc::new(|| {}));
        let mut ev = Events::new(&svc);
        svc.send(Command::RestartServer);
        let error = ev.wait_for(Duration::from_secs(15), |e| matches!(e, Event::Error(_)));
        let Some(Event::Error(text)) = error else { panic!("no error: {:#?}", ev.job_events()) };
        assert!(text.starts_with("Bonsai was not restarted") && text.contains(NO_INSTALL), "{text}");
        assert!(!ev.saw(|e| matches!(e, Event::Status(s) if s.state == ServerState::Stopping)));
        svc.send(Command::Shutdown);
        assert!(svc.wait_shutdown(Duration::from_secs(15)));
    }

    /// A handle dropped without `Shutdown` (a UI panic) still ends the service before it returns.
    #[test]
    fn dropped_handle_shuts_the_service_down() {
        let cfg = AppConfig { bonsai_dir: None, server_url: DEAD_URL.into(), ..AppConfig::default() };
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let svc = spawn(cfg, Arc::clone(&repaint));
        svc.send(Command::Refresh);
        let begun = Instant::now();
        drop(svc);
        assert!(begun.elapsed() < Duration::from_secs(5), "drop waited {:?}", begun.elapsed());
        // The service thread's event senders (and with them the repaint callback) are gone.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&repaint) > 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(Arc::strong_count(&repaint), 1, "the service still runs");
    }

    // ---- live: the user's running Bonsai --------------------------------------------------

    /// Problems found by the live test; it always shuts the service down before reporting them.
    #[derive(Default)]
    struct Problems(Vec<String>);

    impl Problems {
        fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
            if !ok {
                self.0.push(what());
            }
        }
    }

    /// Uses the user's running Bonsai and never starts or stops it (auto-start off; stop-on-exit
    /// on, which must not touch a server this app did not start): a server-URL switch from a dead
    /// address, Ready status with props, token count, a queued job cancelled while another
    /// streams, cancelling a running chat, a tiny write job, then a shutdown that must leave the
    /// server running. Every step reports its problems and the shutdown check always runs.
    #[test]
    #[ignore = "needs the user's Bonsai server on http://127.0.0.1:8080; run with --ignored"]
    fn live_round_trip_leaves_external_server_running() {
        const URL: &str = "http://127.0.0.1:8080";
        let probe = BonsaiClient::new(URL);
        assert_eq!(probe.health(), Health::Ready, "start Bonsai first (start_bonsai.bat)");
        let cfg = AppConfig {
            bonsai_dir: Some(PathBuf::from(r"D:\Bonsai-demo")),
            server_url: URL.into(),
            auto_start: false,
            stop_on_exit: true,
            ..AppConfig::default()
        };
        // Starts on a dead URL; `live_url_switch` points it at the real server.
        let svc = spawn(AppConfig { server_url: DEAD_URL.into(), ..cfg.clone() }, Arc::new(|| {}));
        let mut ev = Events::new(&svc);
        let mut problems = Problems::default();

        live_url_switch(&svc, &mut ev, &cfg, &mut problems);
        if live_ready(&mut ev, &mut problems) {
            live_token_count(&svc, &mut ev, &mut problems);
            live_queue_and_cancel(&svc, &mut ev, &mut problems);
            live_cancel_running(&svc, &mut ev, &mut problems);
            live_write(&svc, &mut ev, &mut problems);
        }

        // Shutdown with stop-on-exit on: the user's server must keep running. Should the service
        // ever claim it, stop-on-exit is switched off first so this test can never stop it.
        let claimed = ev.last_status().is_some_and(|s| s.started_by_us);
        if claimed {
            svc.send(Command::UpdateConfig(Box::new(AppConfig { stop_on_exit: false, ..cfg })));
        }
        svc.send(Command::Shutdown);
        let confirmed = svc.wait_shutdown(Duration::from_secs(30));
        problems.check(!claimed, || "started_by_us = true for a server this test did not start".into());
        problems.check(confirmed, || "no ShutdownComplete".into());
        problems.check(probe.health() == Health::Ready, || "the user's Bonsai stopped".into());
        assert!(problems.0.is_empty(), "live problems:\n{}", problems.0.join("\n"));
    }

    /// Stopped on the dead URL, then a new server URL in `UpdateConfig` (the manager is rebuilt).
    fn live_url_switch(svc: &ServiceHandle, ev: &mut Events<'_>, cfg: &AppConfig, problems: &mut Problems) {
        let stopped =
            ev.wait_for(Duration::from_secs(15), |e| matches!(e, Event::Status(s) if s.state == ServerState::Stopped));
        problems.check(stopped.is_some(), || format!("no Stopped status on the dead URL: {:#?}", ev.last_status()));
        svc.send(Command::UpdateConfig(Box::new(cfg.clone())));
    }

    /// A Ready status from the external server, with props, not claimed as ours.
    fn live_ready(ev: &mut Events<'_>, problems: &mut Problems) -> bool {
        let ready = ev.wait_for(
            Duration::from_secs(30),
            |e| matches!(e, Event::Status(s) if s.state.is_ready() && s.props.is_some()),
        );
        let Some(Event::Status(snap)) = ready else {
            problems.0.push(format!("no Ready status with props: {:#?}", ev.last_status()));
            return false;
        };
        problems.check(!snap.started_by_us, || "the external server shows started_by_us".into());
        problems.check(snap.install_ok, || format!("D:\\Bonsai-demo should be usable: {:?}", snap.install));
        problems.check(snap.props.as_ref().is_some_and(|p| p.n_ctx > 0), || format!("props: {:?}", snap.props));
        problems.check(snap.auto_stop_in_secs.is_none(), || "idle auto-stop for a server we did not start".into());
        !snap.started_by_us
    }

    fn live_token_count(svc: &ServiceHandle, ev: &mut Events<'_>, problems: &mut Problems) {
        svc.send(Command::CountTokens { id: 7, text: "Hello Bonsai, count these tokens.".into() });
        let count = ev.wait_for(Duration::from_secs(15), |e| matches!(e, Event::TokenCount { id: 7, .. }));
        let counted = matches!(count, Some(Event::TokenCount { count: n, .. }) if n > 0);
        problems.check(counted, || format!("token count: {count:?}"));
    }

    /// Two chats: the second is cancelled while it waits behind the first, which streams to the end.
    fn live_queue_and_cancel(svc: &ServiceHandle, ev: &mut Events<'_>, problems: &mut Problems) {
        svc.send(Command::Submit { id: 1, kind: chat_job("Reply with one word: ready", 20) });
        svc.send(Command::Submit { id: 2, kind: chat_job("Reply with one word: second", 20) });
        svc.send(Command::Cancel(2));
        let done = ev.wait_for(Duration::from_secs(120), |e| {
            matches!(e, Event::ChatDone { id: 1, .. } | Event::JobFailed { id: 1, .. })
        });
        let Some(Event::ChatDone { result, .. }) = done else {
            problems.0.push(format!("chat 1 did not finish: {done:?}\n{:#?}", ev.job_events()));
            return;
        };
        problems.check(!result.content.trim().is_empty(), || format!("empty chat answer: {result:?}"));
        let reason_ok = result.finish_reason == "stop" || result.finish_reason == "length";
        problems.check(reason_ok, || format!("finish_reason: {result:?}"));
        let queued = ev.saw(|e| *e == Event::JobQueued(1)) && ev.saw(|e| *e == Event::JobQueued(2));
        problems.check(queued, || "JobQueued missing".into());
        problems.check(ev.saw(|e| *e == Event::JobCancelled(2)), || {
            format!("queued job 2 not cancelled: {:#?}", ev.job_events())
        });
        problems.check(!ev.saw(|e| matches!(e, Event::ChatDone { id: 2, .. })), || "cancelled job 2 ran".into());
        for wanted in [phase::PROMPT, phase::WRITING] {
            let seen = ev.saw(|e| matches!(e, Event::JobPhase { id: 1, phase: p } if p == wanted));
            problems.check(seen, || format!("phase {wanted:?} missing for job 1"));
        }
        let streamed: String = ev
            .seen
            .iter()
            .filter_map(|e| match e {
                Event::JobDelta { id: 1, content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        problems.check(streamed.trim() == result.content.trim(), || {
            format!("deltas {streamed:?} do not add up to the answer {:?}", result.content)
        });
    }

    /// Cancel a running chat after its first text.
    fn live_cancel_running(svc: &ServiceHandle, ev: &mut Events<'_>, problems: &mut Problems) {
        svc.send(Command::Submit { id: 4, kind: chat_job("Count from 1 to 100, one number per line.", 60) });
        let first = ev.wait_for(Duration::from_secs(120), |e| {
            matches!(e, Event::JobDelta { id: 4, .. } | Event::ChatDone { id: 4, .. } | Event::JobFailed { id: 4, .. })
        });
        problems.check(matches!(first, Some(Event::JobDelta { .. })), || format!("chat 4 first event: {first:?}"));
        svc.send(Command::Cancel(4));
        let end = ev.wait_for(Duration::from_secs(60), |e| {
            matches!(e, Event::JobCancelled(4) | Event::ChatDone { id: 4, .. } | Event::JobFailed { id: 4, .. })
        });
        problems.check(matches!(end, Some(Event::JobCancelled(4))), || format!("chat 4 not cancelled: {end:?}"));
        let idle = ev.wait_for(Duration::from_secs(10), |e| matches!(e, Event::Status(s) if !s.busy && s.queued == 0));
        problems.check(idle.is_some(), || "no idle status after the cancel".into());
    }

    /// A tiny write job (40 tokens: a truncated prompt, not saved to history).
    fn live_write(svc: &ServiceHandle, ev: &mut Events<'_>, problems: &mut Problems) {
        let mut job = WriteJob::from_text(
            Mode::R2v,
            "One woman in a red coat walks through light rain at night. 6 seconds, one shot.",
            GenParams { max_tokens: 40, ..GenParams::prompt_writer() },
        );
        job.save_history = false;
        svc.send(Command::Submit { id: 3, kind: JobKind::Write(job) });
        let written = ev.wait_for(Duration::from_secs(180), |e| {
            matches!(e, Event::WriteDone { id: 3, .. } | Event::JobFailed { id: 3, .. })
        });
        match written {
            Some(Event::WriteDone { outcome, .. }) => {
                problems.check(!outcome.prompt.trim().is_empty(), || format!("empty prompt: {outcome:?}"));
                problems.check(outcome.history_id.is_none(), || "saved to history with save_history off".into());
            }
            Some(Event::JobFailed { error, .. }) if error.starts_with("Bonsai returned an empty answer") => {}
            other => problems.0.push(format!("write job: {other:?}")),
        }
    }
}
