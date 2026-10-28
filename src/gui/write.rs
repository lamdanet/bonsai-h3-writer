//! Write workspace: request builder -> Bonsai -> H3 prompt with lint report.
//!
//! Left toolbar: Mode, Templates, Tips. Centre: the **Request** area (Builder / Text tabs and the
//! Write button) above the **Prompt** area (reasoning, the prompt, Copy / Save .txt). Right
//! N-panel: Bonsai, Generation, Check, Fix, Checklist, Last run.
//!
//! A job streams into the Prompt area (read-only while it runs); a failed or cancelled job puts
//! the previous prompt back. A fix is a follow-up job on the current (maybe edited) prompt.
//! Replacing the work on screen (Open in Write, a new answer over an edited prompt) offers Undo
//! for a few seconds. The form, the request text and the prompt survive a restart
//! (`draft.json` in the config folder). `BH3_DEMO=1` fills the form and the prompt instead
//! (screenshots; no draft is read or saved).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bh3::client::Timings;
use bh3::config::AppConfig;
use bh3::gen::{GenParams, Mode, ThinkingMode};
use bh3::history::HistoryEntry;
use bh3::lengths;
use bh3::library::{Library, Template};
use bh3::lint::{self, LintContext, LintIssue, LintReport, Severity};
use bh3::request::{
    self, CameraPreset, DialogueLine, RefImage, RefRole, RequestSpec, StylePreset, IMAGE_SECONDS, IMAGE_TOKENS,
};
use bh3::server::ServerState;
use bh3::service::{phase, Command, Event, JobId, JobKind, StatusSnapshot};
use bh3::writer::{self, FollowUp, WriteJob, WriteOutcome};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Key, KeyboardShortcut, Layout, Margin, Modifiers, Rect,
    RichText, ScrollArea, Sense, Stroke, TextEdit, TextStyle, Ui,
};
use serde::{Deserialize, Serialize};

use super::app::{AppState, ToastKind, Workspace};
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Thumbnail size of a reference picture.
const THUMB: f32 = 56.0;
/// Box around one reference (like Blender's darker `layout.box()`).
const CARD_FILL: Color32 = Color32::from_rgb(44, 44, 44);
const CARD_LINE: Color32 = Color32::from_rgb(36, 36, 36);
/// Hover fill of clickable list rows (lint issues).
const ROW_HOVER: Color32 = Color32::from_rgb(62, 62, 62);
/// Height of the bar with the Write button under the request.
const ACTION_BAR_H: f32 = 46.0;
/// Width of the Write / Cancel button.
const ACTION_BUTTON_W: f32 = 190.0;
/// Id of the prompt editor (the Check panel moves its cursor).
const PROMPT_EDITOR: &str = "write-prompt-editor";
/// Id of the custom fix field (Ctrl+Enter there sends the fix).
const FIX_FIELD: &str = "write-fix-field";
/// The prompt shown with `BH3_DEMO=1` (a tested r2v answer: 6 s, 2 refs, one take).
const DEMO_PROMPT: &str = include_str!("../../tests/fixtures/good_r2v_romance_1.txt");
/// The Write draft in the config folder (see [`Draft`]).
const DRAFT_FILE: &str = "draft.json";
/// Seconds between draft checks; it is written only when it changed.
const DRAFT_EVERY: f64 = 2.0;
/// Seconds the Undo offer stays after the work on screen was replaced.
const UNDO_SECS: f64 = 10.0;

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

/// Tab of the Request area.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RequestTab {
    #[default]
    Builder,
    Text,
}

/// The request behind the current prompt (the earlier turn of a follow-up fix).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct SentRequest {
    mode: Mode,
    text: String,
    /// The form when it was sent (its ticked pictures go along with fixes, also with own wording).
    spec: Option<RequestSpec>,
    /// `text` was built from `spec` (then the form is saved to History with the fixes).
    form_text: bool,
    lint_ctx: LintContext,
}

/// What the Prompt area showed before a job started (put back when it fails or is cancelled).
#[derive(Clone, Debug, Default)]
struct PromptSnapshot {
    prompt: String,
    reasoning: String,
    lint: Option<LintReport>,
    prompt_ctx: Option<LintContext>,
    /// The prompt had hand edits.
    edited: bool,
}

/// A job of this workspace that has not finished yet.
struct RunningJob {
    id: JobId,
    job: WriteJob,
    /// The request the prompt is written from (becomes `last_sent` when it succeeds); `None` for a
    /// fix of a pasted prompt.
    sent: Option<SentRequest>,
    phase: String,
    /// `AppState::time` when it was submitted.
    started: f64,
    cancel_requested: bool,
    previous: PromptSnapshot,
}

impl RunningJob {
    fn is_fix(&self) -> bool {
        self.job.followup.is_some()
    }

    fn status_text(&self, now: f64) -> String {
        let secs = (now - self.started).max(0.0) as u64;
        let what = if self.is_fix() { "Fix: " } else { "" };
        format!("{what}{} {}", self.phase, widgets::fmt_secs(secs))
    }
}

/// Where the numbers of the Last run panel come from.
#[derive(Clone, Debug, PartialEq)]
enum RunSource {
    Written,
    Fixed,
    /// Opened from History (its date).
    Opened(String),
}

/// Numbers of the last finished job (or of the entry opened from History).
#[derive(Clone, Debug)]
struct LastRun {
    source: RunSource,
    history_id: Option<String>,
    timings: Option<Timings>,
    finish_reason: String,
    elapsed_ms: Option<u64>,
    max_tokens: u32,
}

#[derive(Default)]
pub struct WriteState {
    /// The form was set up (from the draft or the settings) or by `load_entry`.
    initialized: bool,
    spec: RequestSpec,
    tab: RequestTab,
    /// Request text; follows `spec.build_text()` until it is edited.
    text: String,
    edited: bool,
    /// The edited text started as the form's own text (the form fills in the facts the text does
    /// not state); false for a template, a typed request from History or a text typed from scratch.
    text_from_form: bool,
    /// Check facts of a typed request opened from History (its form was not saved).
    text_ctx: Option<LintContext>,
    prompt: String,
    reasoning: String,
    lint: Option<LintReport>,
    /// What the prompt is checked against (the job's context); `None` = the request's.
    prompt_ctx: Option<LintContext>,
    /// The prompt was edited by hand since it was written or loaded.
    prompt_edited: bool,
    job: Option<RunningJob>,
    /// Jobs still running after `load_entry` replaced the view (their answers go to History).
    background: Vec<RunningJob>,
    /// The answer of a finished background job, until it is shown or dismissed.
    earlier: Option<Box<Earlier>>,
    /// The request the current prompt was written from (`None` for a pasted prompt).
    last_sent: Option<SentRequest>,
    /// History id of the current prompt (parent of the next fix).
    parent_id: Option<String>,
    last_run: Option<LastRun>,
    fix_text: String,
    focus_fix: bool,
    /// One level of undo after the work on screen was replaced.
    undo: Option<Undo>,
    /// Ticked checklist items (`section \n item`).
    ticks: HashSet<String>,
    /// r2v roles and pin before switching to f2v (restored when switching back).
    r2v_roles: Vec<RefRole>,
    r2v_pinned: bool,
    /// Select this 1-based line in the prompt editor (from the Check panel).
    jump_to_line: Option<usize>,
    /// Generation settings changed: save when the pointer is released.
    config_dirty: bool,
    /// The text this workspace last put into `app.activity`.
    activity: Option<String>,
    draft: DraftSaver,
}

impl WriteState {
    /// Load a history entry (its form when saved, else its request text, and its prompt) for editing
    /// or follow-up fixes. Called by the History workspace ("Open in Write"). Work that was on
    /// screen can be brought back with Undo for a few seconds.
    pub fn load_entry(&mut self, entry: &bh3::history::HistoryEntry) {
        let saved = (self.initialized && self.has_work()).then(|| self.snapshot());
        self.initialized = true;
        if let Some(job) = self.job.take() {
            // It keeps running and its answer is saved to History (and offered with Show).
            self.background.push(job);
        }
        let ctx = writer::entry_lint_context(entry);
        self.spec = match &entry.spec {
            Some(spec) => spec.clone(),
            None => default_spec(entry.mode, entry.duration_s.unwrap_or(0)),
        };
        self.text = entry.original_request().to_string();
        self.edited = entry.spec.is_none();
        self.text_from_form = entry.spec.is_some();
        self.text_ctx = entry.spec.is_none().then(|| ctx.clone());
        self.tab = if entry.spec.is_some() { RequestTab::Builder } else { RequestTab::Text };
        self.prompt = entry.output.clone();
        self.reasoning = entry.reasoning.clone();
        self.lint = (!self.prompt.trim().is_empty()).then(|| lint::lint(&self.prompt, &ctx));
        self.prompt_ctx = Some(ctx.clone());
        self.prompt_edited = false;
        self.parent_id = Some(entry.id.clone()).filter(|id| !id.is_empty());
        self.last_sent = Some(SentRequest {
            mode: entry.mode,
            text: entry.original_request().to_string(),
            spec: entry.spec.clone(),
            form_text: entry.spec.is_some(),
            lint_ctx: ctx,
        });
        self.last_run = Some(LastRun {
            source: RunSource::Opened(entry.short_date()),
            history_id: self.parent_id.clone(),
            timings: entry.timings.clone(),
            finish_reason: String::new(),
            elapsed_ms: None,
            max_tokens: entry.params.max_tokens,
        });
        self.fix_text.clear();
        self.r2v_roles.clear();
        self.jump_to_line = None;
        self.undo = saved.map(|saved| {
            let title = if entry.title.trim().is_empty() { "the entry" } else { entry.title.trim() };
            Undo::new(format!("Opened \u{201c}{title}\u{201d} from History"), true, saved)
        });
    }

    /// Keep the request text in step with the form until it is edited.
    fn sync_text(&mut self) {
        if !self.edited {
            self.text_from_form = true;
            let built = self.spec.build_text();
            if built != self.text {
                self.text = built;
            }
        }
    }

    fn rebuild_text(&mut self) {
        self.edited = false;
        self.text_from_form = true;
        self.text_ctx = None;
        self.text = self.spec.build_text();
    }

    fn use_template(&mut self, template: &Template) {
        self.text = template.text.clone();
        self.edited = true;
        self.text_from_form = false;
        self.text_ctx = None;
        self.tab = RequestTab::Text;
    }

    /// Check facts of the edited request text: what the text states, the rest from the form while
    /// the text started as the form's own text (or from the History entry it was opened from).
    fn text_lint_context(&self) -> LintContext {
        let mode = self.spec.mode;
        let stated = request::context_from_text(mode, &self.text);
        if self.text_from_form {
            request::fill_missing(stated, &self.spec.lint_context())
        } else if let Some(entry_ctx) = self.text_ctx.as_ref().filter(|c| c.mode == mode) {
            request::fill_missing(stated, entry_ctx)
        } else {
            stated
        }
    }

    /// Check facts of the request as it would be sent now.
    fn request_lint_context(&self) -> LintContext {
        if self.edited {
            self.text_lint_context()
        } else {
            self.spec.lint_context()
        }
    }

    /// What the prompt is checked against: its job's facts, else the request's (a pasted prompt).
    fn prompt_context(&self) -> LintContext {
        self.prompt_ctx.clone().unwrap_or_else(|| self.request_lint_context())
    }

    /// Check the prompt again (after an edit).
    fn relint(&mut self) {
        if self.prompt.trim().is_empty() {
            self.lint = None;
            return;
        }
        self.lint = Some(lint::lint(&self.prompt, &self.prompt_context()));
    }

    fn clear_prompt(&mut self) {
        self.prompt.clear();
        self.reasoning.clear();
        self.lint = None;
        self.forget_prompt_source();
        self.jump_to_line = None;
    }

    /// The prompt on screen is not the answer it was (cleared, emptied or pasted over): its
    /// request, check facts and History parent no longer apply.
    fn forget_prompt_source(&mut self) {
        self.prompt_ctx = None;
        self.parent_id = None;
        self.last_sent = None;
        self.prompt_edited = false;
    }

    /// Something worth keeping: a prompt (or a job writing one), own request wording, a fix being
    /// typed or a filled-in form.
    fn has_work(&self) -> bool {
        self.job.is_some()
            || !self.prompt.trim().is_empty()
            || (self.edited && !self.text.trim().is_empty())
            || !self.fix_text.trim().is_empty()
            || self.spec != default_spec(self.spec.mode, self.spec.duration_s)
    }

    /// The content of the workspace (with the prompt from before a running job: its stream is
    /// not finished).
    fn snapshot(&self) -> Draft {
        let (prompt, reasoning, prompt_ctx, prompt_edited) = match &self.job {
            Some(job) => {
                let p = &job.previous;
                (p.prompt.clone(), p.reasoning.clone(), p.prompt_ctx.clone(), p.edited)
            }
            None => (self.prompt.clone(), self.reasoning.clone(), self.prompt_ctx.clone(), self.prompt_edited),
        };
        Draft {
            spec: self.spec.clone(),
            tab: self.tab,
            text: self.text.clone(),
            edited: self.edited,
            text_from_form: self.text_from_form,
            text_ctx: self.text_ctx.clone(),
            prompt,
            reasoning,
            prompt_ctx,
            prompt_edited,
            parent_id: self.parent_id.clone(),
            last_sent: self.last_sent.clone(),
            fix_text: self.fix_text.clone(),
            last_run: self.last_run.clone(),
        }
    }

    /// Put a snapshot back: all of it, or (`whole` false) only the prompt and what it was
    /// written from.
    fn restore(&mut self, saved: Draft, whole: bool) {
        if whole {
            self.spec = saved.spec;
            self.tab = saved.tab;
            self.text = saved.text;
            self.edited = saved.edited;
            self.text_from_form = saved.text_from_form;
            self.text_ctx = saved.text_ctx;
            self.fix_text = saved.fix_text;
            self.r2v_roles.clear();
        }
        self.prompt = saved.prompt;
        self.reasoning = saved.reasoning;
        self.prompt_ctx = saved.prompt_ctx;
        self.prompt_edited = saved.prompt_edited;
        self.parent_id = saved.parent_id;
        self.last_sent = saved.last_sent;
        self.last_run = saved.last_run;
        self.jump_to_line = None;
        self.relint();
    }

    /// Undo: put back what was replaced.
    fn apply_undo(&mut self) {
        if let Some(undo) = self.undo.take() {
            self.restore(*undo.saved, undo.whole);
        }
    }

    /// Show the answer of a finished background job in the Prompt area (Undo brings back the
    /// prompt it replaces).
    fn show_earlier(&mut self) -> Vec<(ToastKind, String)> {
        let Some(earlier) = self.earlier.take() else { return Vec::new() };
        let Earlier { mut job, outcome } = *earlier;
        let saved = (!self.prompt.trim().is_empty()).then(|| self.snapshot());
        // What the job replaced back then is not on screen any more.
        job.previous = PromptSnapshot::default();
        let (toasts, _) = self.finish(job, &outcome);
        if let Some(saved) = saved {
            self.undo = Some(Undo::new("Showing the earlier prompt".to_string(), false, saved));
        }
        toasts
    }

    /// Switch r2v / f2v. f2v: Ref 1 becomes the first frame and Ref 2 the last frame; back in r2v
    /// the earlier roles return and roles r2v cannot use are fixed.
    fn set_mode(&mut self, mode: Mode) {
        if self.spec.mode == mode {
            return;
        }
        let spec = &mut self.spec;
        match mode {
            Mode::F2v => {
                self.r2v_roles = spec.refs.iter().map(|r| r.role).collect();
                self.r2v_pinned = spec.first_frame_pinned;
                if spec.refs.is_empty() {
                    spec.refs.push(RefImage { role: RefRole::FirstFrame, ..RefImage::default() });
                }
                for (i, r) in spec.refs.iter_mut().enumerate() {
                    match i {
                        0 => r.role = RefRole::FirstFrame,
                        1 => r.role = RefRole::LastFrame,
                        _ => {}
                    }
                }
            }
            Mode::R2v => {
                let saved = std::mem::take(&mut self.r2v_roles);
                let restored = !saved.is_empty();
                for (i, r) in spec.refs.iter_mut().enumerate() {
                    let forced = (i == 0 && r.role == RefRole::FirstFrame) || (i == 1 && r.role == RefRole::LastFrame);
                    if let (true, Some(&role)) = (forced, saved.get(i)) {
                        r.role = role;
                    }
                    let first_frame_elsewhere = r.role == RefRole::FirstFrame && i > 0;
                    if !r.role.allowed_in(Mode::R2v) || first_frame_elsewhere {
                        r.role = RefRole::Keyframe;
                    }
                }
                let ref1_frame = spec.refs.first().is_some_and(|r| r.role == RefRole::FirstFrame);
                spec.first_frame_pinned = if restored { self.r2v_pinned && ref1_frame } else { ref1_frame };
            }
        }
        spec.mode = mode;
    }

    /// A finished job: show its prompt, lint and numbers. Returns the toasts and the timings.
    fn finish(&mut self, job: RunningJob, outcome: &WriteOutcome) -> (Vec<(ToastKind, String)>, Option<Timings>) {
        let result = &outcome.result;
        let mut toasts = Vec::new();
        let is_fix = job.is_fix();
        let max_tokens = job.job.params.max_tokens;
        if result.cancelled {
            let restored = self.end_without_prompt(job);
            toasts.push((ToastKind::Info, cancelled_text(restored)));
            return (toasts, None);
        }
        // Hand edits are saved nowhere: a new answer over them can be undone.
        let replaced = (job.previous.edited && !job.previous.prompt.trim().is_empty()).then(|| {
            let mut saved = self.snapshot();
            saved.prompt = job.previous.prompt.clone();
            saved.reasoning = job.previous.reasoning.clone();
            saved.prompt_ctx = job.previous.prompt_ctx.clone();
            saved.prompt_edited = true;
            saved
        });
        self.last_run = Some(LastRun {
            source: if is_fix { RunSource::Fixed } else { RunSource::Written },
            history_id: outcome.history_id.clone(),
            timings: result.timings.clone(),
            finish_reason: result.finish_reason.clone(),
            elapsed_ms: Some(result.elapsed_ms),
            max_tokens,
        });
        if outcome.prompt.trim().is_empty() {
            let restored = self.end_without_prompt(job);
            let mut text = "Bonsai answered without a prompt".to_string();
            if result.finish_reason == "length" {
                text.push_str(&format!(" (it used all {max_tokens} tokens: turn thinking off or raise Max tokens)"));
            }
            if restored {
                text.push_str(" - the previous prompt is back");
            }
            toasts.push((ToastKind::Warn, text));
            return (toasts, result.timings.clone());
        }
        self.prompt = outcome.prompt.clone();
        self.reasoning = outcome.reasoning.clone();
        self.lint = Some(outcome.lint.clone());
        self.prompt_ctx = Some(writer::lint_context_for(&job.job, &outcome.prompt));
        self.prompt_edited = false;
        self.parent_id = outcome.history_id.clone();
        self.last_sent = job.sent;
        // A fix typed while this one ran stays in the field.
        if job.job.followup.as_ref().is_some_and(|f| f.fix.trim() == self.fix_text.trim()) {
            self.fix_text.clear();
        }
        if let Some(saved) = replaced {
            let message = if is_fix { "Fix applied to your edited prompt" } else { "Your edited prompt was replaced" };
            self.undo = Some(Undo::new(message.to_string(), false, saved));
        }
        let errors = outcome.lint.errors();
        let what = if is_fix { "Fix applied" } else { "Prompt written" };
        let kind = if errors > 0 { ToastKind::Warn } else { ToastKind::Ok };
        toasts.push((kind, format!("{what} - score {}", outcome.lint.score)));
        if result.finish_reason == "length" {
            toasts.push((
                ToastKind::Warn,
                format!("Bonsai stopped at the {max_tokens} token limit: the prompt may be cut off (raise Max tokens)"),
            ));
        }
        (toasts, result.timings.clone())
    }

    /// A job ended without a usable prompt: put the previous prompt back (returns true), or keep
    /// and check what was streamed when there was none.
    fn end_without_prompt(&mut self, job: RunningJob) -> bool {
        if job.previous.prompt.trim().is_empty() {
            self.prompt_ctx = Some(job.job.lint_ctx.clone());
            self.relint();
            return false;
        }
        let previous = job.previous;
        self.prompt = previous.prompt;
        self.reasoning = previous.reasoning;
        self.lint = previous.lint;
        self.prompt_ctx = previous.prompt_ctx;
        self.prompt_edited = previous.edited;
        true
    }
}

/// One level of undo for work on screen that was replaced (Open in Write, a new answer over an
/// edited prompt, Show of an earlier prompt).
struct Undo {
    /// What happened, e.g. `Opened "..." from History`.
    message: String,
    /// Put back the whole workspace (else only the prompt and what it was written from).
    whole: bool,
    saved: Box<Draft>,
    /// `AppState::time` when the offer was first shown (it goes away `UNDO_SECS` later).
    shown_at: Option<f64>,
}

impl Undo {
    fn new(message: String, whole: bool, saved: Draft) -> Self {
        Self { message, whole, saved: Box::new(saved), shown_at: None }
    }
}

/// A job that `load_entry` moved to the background and that finished with a prompt.
struct Earlier {
    job: RunningJob,
    outcome: WriteOutcome,
}

/// The content of the Write workspace: saved as `draft.json` (restored on the next start) and
/// kept for Undo.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Draft {
    spec: RequestSpec,
    tab: RequestTab,
    text: String,
    edited: bool,
    text_from_form: bool,
    text_ctx: Option<LintContext>,
    prompt: String,
    reasoning: String,
    prompt_ctx: Option<LintContext>,
    prompt_edited: bool,
    parent_id: Option<String>,
    /// The follow-up context of the prompt.
    last_sent: Option<SentRequest>,
    fix_text: String,
    /// Kept for Undo only.
    #[serde(skip)]
    last_run: Option<LastRun>,
}

/// Writes the draft when it changed, checked every `DRAFT_EVERY` seconds and on exit.
#[derive(Default)]
struct DraftSaver {
    /// `config_dir()\draft.json`; `None` = no draft (demo, tests).
    path: Option<PathBuf>,
    /// JSON of the draft on disk (written or restored).
    saved: String,
    /// `AppState::time` of the last check.
    checked: f64,
    /// A failed save was reported (once).
    warned: bool,
}

/// The draft in `path`: `None` when there is none, or when it is not a draft (then it is kept as
/// `draft.json.bad` and a new one is started).
fn read_draft(path: &Path) -> Option<Draft> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<Draft>(&bytes) {
        Ok(draft) => Some(draft),
        Err(e) => {
            let bad = path.with_extension("json.bad");
            eprintln!("draft {}: {e} (kept as {})", path.display(), bad.display());
            if let Err(e) = std::fs::rename(path, &bad) {
                eprintln!("draft: could not rename it: {e}");
            }
            None
        }
    }
}

/// Write the draft through a temporary file, so a crash never leaves half a draft.
fn write_draft(path: &Path, json: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn cancelled_text(restored: bool) -> String {
    if restored {
        "Cancelled - the previous prompt is back".to_string()
    } else {
        "Cancelled".to_string()
    }
}

/// A fresh form for `mode` (Ref 1 is the first frame in f2v).
fn default_spec(mode: Mode, duration_s: u32) -> RequestSpec {
    let mut spec =
        RequestSpec { mode, duration_s: if duration_s == 0 { 6 } else { duration_s }, ..RequestSpec::default() };
    if mode == Mode::F2v {
        for r in &mut spec.refs {
            r.role = RefRole::FirstFrame;
        }
    }
    spec
}

// ---------------------------------------------------------------------------------------------
// Frame entry points
// ---------------------------------------------------------------------------------------------

/// What the user asked for this frame (applied after the panels are drawn).
#[derive(Default)]
struct Actions {
    submit: bool,
    send_fix: bool,
    cancel: bool,
    cancel_background: Option<JobId>,
    start_server: bool,
    stop_server: bool,
    save_config: bool,
    save_txt: bool,
    copy_prompt: bool,
    /// Copied with the keyboard (confirm with a toast).
    copy_by_key: bool,
    clear_prompt: bool,
    open: Option<Workspace>,
    undo: bool,
    dismiss_undo: bool,
    show_earlier: bool,
    dismiss_earlier: bool,
}

/// A line next to the Write button.
struct Hint {
    text: String,
    color: Color32,
    /// Workspace that solves it (an "Open" button).
    open: Option<Workspace>,
}

/// Facts the panels need, computed once per frame.
struct View {
    now: f64,
    running: bool,
    /// Bonsai can take a job (running, starting, or auto-start can start it).
    can_run: bool,
    /// `can_run` and the request is not empty.
    can_write: bool,
    hint: Option<Hint>,
}

fn view_of(app: &AppState) -> View {
    let status = &app.status;
    let known = *status != StatusSnapshot::default();
    let dim = |text: &str| Some(Hint { text: text.to_string(), color: Palette::TEXT_DIM, open: None });
    let (can_run, hint) = match &status.state {
        _ if !known => (true, None),
        ServerState::Ready => (true, None),
        ServerState::Starting | ServerState::Loading => {
            (true, dim("Bonsai is starting: the prompt is written as soon as it is ready"))
        }
        _ if !status.install_ok => (
            false,
            Some(Hint {
                text: "Choose the Bonsai folder first".to_string(),
                color: Palette::WARN,
                open: Some(Workspace::Server),
            }),
        ),
        _ if app.config.auto_start => (true, dim("Bonsai starts automatically (~7.7 GB VRAM: close ComfyUI first)")),
        _ => (
            false,
            Some(Hint {
                text: "Bonsai is not running and auto-start is off: press Start in the Bonsai panel".to_string(),
                color: Palette::WARN,
                open: None,
            }),
        ),
    };
    let ws = &app.write;
    let empty = ws.edited && ws.text.trim().is_empty();
    let (can_write, hint) = if empty {
        (
            false,
            Some(Hint {
                text: "The request text is empty: type a request or rebuild it from the form".to_string(),
                color: Palette::WARN,
                open: None,
            }),
        )
    } else {
        (can_run, hint)
    };
    View { now: app.time, running: ws.job.is_some(), can_run, can_write, hint }
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    if !app.write.initialized {
        init(app);
    }
    let mut acts = Actions::default();
    if !app.show_setup {
        read_shortcuts(app, ctx, &mut acts);
    }
    app.write.sync_text();
    // The Undo offer counts from when it is first seen.
    let now = app.time;
    if let Some(undo) = app.write.undo.as_mut() {
        if now - *undo.shown_at.get_or_insert(now) > UNDO_SECS {
            app.write.undo = None;
        }
    }
    let view = view_of(app);

    egui::SidePanel::left("write-left")
        .resizable(true)
        .default_width(250.0)
        .width_range(200.0..=380.0)
        .frame(side_frame())
        .show(ctx, |ui| left_panel(ui, &mut app.write, &app.library));

    egui::SidePanel::right("write-right")
        .resizable(true)
        .default_width(330.0)
        .width_range(280.0..=480.0)
        .frame(side_frame())
        .show(ctx, |ui| {
            let mut right = RightPanel {
                ws: &mut app.write,
                config: &mut app.config,
                status: &app.status,
                library: &app.library,
                view: &view,
                acts: &mut acts,
            };
            right.show(ui);
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(Palette::GAP).inner_margin(Margin::same(1)))
        .show(ctx, |ui| centre(ui, &mut app.write, &app.config, &view, &mut acts));

    apply_actions(app, ctx, acts, &view);
}

pub fn on_event(app: &mut AppState, ev: &bh3::service::Event) {
    let ws = &mut app.write;
    let current = ws.job.as_ref().map(|j| j.id);
    let mut toasts: Vec<(ToastKind, String)> = Vec::new();
    let mut timings = None;
    match ev {
        Event::JobPhase { id, phase } if Some(*id) == current => {
            if let Some(job) = ws.job.as_mut().filter(|j| !j.cancel_requested) {
                job.phase = phase.clone();
            }
        }
        Event::JobDelta { id, reasoning, content } if Some(*id) == current => {
            ws.reasoning.push_str(reasoning);
            ws.prompt.push_str(content);
        }
        Event::WriteDone { id, outcome } if Some(*id) == current => {
            if let Some(job) = ws.job.take() {
                (toasts, timings) = ws.finish(job, outcome);
            }
        }
        Event::JobFailed { id, error } if Some(*id) == current => {
            if let Some(job) = ws.job.take() {
                ws.end_without_prompt(job);
            }
            toasts.push((ToastKind::Error, error.clone()));
        }
        Event::JobCancelled(id) if Some(*id) == current => {
            let restored = ws.job.take().is_some_and(|job| ws.end_without_prompt(job));
            toasts.push((ToastKind::Info, cancelled_text(restored)));
        }
        Event::WriteDone { id, outcome } if ws.background.iter().any(|j| j.id == *id) => {
            let job = take_background(&mut ws.background, *id);
            if !outcome.result.cancelled {
                let saved = if outcome.history_id.is_some() { " and saved to History" } else { "" };
                let text = format!("An earlier prompt finished (score {}){saved}", outcome.lint.score);
                toasts.push((ToastKind::Info, text));
                timings = outcome.result.timings.clone();
                // Kept for Show (with history off it is nowhere else).
                if let Some(job) = job.filter(|_| !outcome.prompt.trim().is_empty()) {
                    ws.earlier = Some(Box::new(Earlier { job, outcome: (**outcome).clone() }));
                }
            }
        }
        Event::JobFailed { id, error } if ws.background.iter().any(|j| j.id == *id) => {
            take_background(&mut ws.background, *id);
            toasts.push((ToastKind::Error, error.clone()));
        }
        Event::JobCancelled(id) if ws.background.iter().any(|j| j.id == *id) => {
            take_background(&mut ws.background, *id);
        }
        _ => return,
    }
    if timings.is_some() {
        app.last_timings = timings;
    }
    for (kind, text) in toasts {
        app.toast(kind, text);
    }
    update_activity(app);
}

fn take_background(background: &mut Vec<RunningJob>, id: JobId) -> Option<RunningJob> {
    let i = background.iter().position(|j| j.id == id)?;
    Some(background.remove(i))
}

/// Set up the workspace (at start): the draft of the last session, else the form from the
/// settings; the demo with `BH3_DEMO=1` (then no draft is read or saved).
pub fn init(app: &mut AppState) {
    if app.write.initialized {
        return;
    }
    let ws = &mut app.write;
    ws.initialized = true;
    ws.spec = default_spec(app.config.last_mode, app.config.default_duration);
    ws.text = ws.spec.build_text();
    ws.text_from_form = true;
    if widgets::demo_mode() {
        load_demo(app);
        return;
    }
    let path = bh3::config::config_dir().join(DRAFT_FILE);
    if let Some(draft) = read_draft(&path) {
        ws.restore(draft, true);
    }
    ws.draft.saved = serde_json::to_string_pretty(&ws.snapshot()).unwrap_or_default();
    ws.draft.path = Some(path);
}

/// Save the draft when it changed (at most every `DRAFT_EVERY` seconds; `now` forces a check, on exit).
pub fn autosave(app: &mut AppState, now: bool) {
    let time = app.time;
    let ws = &mut app.write;
    if ws.draft.path.is_none() || (!now && time - ws.draft.checked < DRAFT_EVERY) {
        return;
    }
    ws.draft.checked = time;
    let Some(path) = ws.draft.path.clone() else { return };
    let json = match serde_json::to_string_pretty(&ws.snapshot()) {
        Ok(json) if json != ws.draft.saved => json,
        _ => return,
    };
    match write_draft(&path, &json) {
        Ok(()) => ws.draft.saved = json,
        Err(e) if !ws.draft.warned => {
            ws.draft.warned = true;
            app.toast(ToastKind::Warn, format!("Could not save the Write draft to {}: {e}", path.display()));
        }
        Err(_) => {}
    }
}

/// A realistic r2v example: two refs described in words, 6 s one take, film look, with a tested
/// prompt, its check and plausible numbers of the last run.
fn load_demo(app: &mut AppState) {
    let spec = RequestSpec {
        mode: Mode::R2v,
        refs: vec![
            RefImage {
                role: RefRole::Person,
                description: "a young woman in her twenties, long black hair in a high bun with a few loose strands, \
                              fair skin, soft dark eyes, small drop earrings and a thin gold necklace"
                    .to_string(),
                image_path: None,
                send_image: false,
            },
            RefImage {
                role: RefRole::Place,
                description: "an empty sunlit corner cafe with tall windows, light oak tables, rattan chairs and a \
                              white marble counter"
                    .to_string(),
                image_path: None,
                send_image: false,
            },
        ],
        duration_s: 6,
        cuts: 0,
        action: "she walks to the window table, sits down across from the young man and smiles at him".to_string(),
        outfit: "a white linen sundress".to_string(),
        style: StylePreset::Film35mm,
        camera: CameraPreset::PushIn,
        dialogue: vec![DialogueLine { speaker: "she".to_string(), words: "Good morning.".to_string() }],
        extra_people: "a young man with short dark hair and light stubble, light-blue linen shirt".to_string(),
        notes: "Soft morning light through the windows.".to_string(),
        ..RequestSpec::default()
    };
    let timings = Timings {
        prompt_n: 186,
        cache_n: 3_354,
        prompt_ms: 702.0,
        prompt_per_second: 265.0,
        predicted_n: 731,
        predicted_ms: 21_480.0,
        predicted_per_second: 34.0,
    };
    let prompt = DEMO_PROMPT.replace("\r\n", "\n");
    let mut entry = HistoryEntry::new(Mode::R2v, &spec.build_text(), prompt.trim());
    entry.title = spec.title();
    entry.duration_s = Some(spec.duration_s);
    entry.timings = Some(timings.clone());
    entry.spec = Some(spec);
    let ws = &mut app.write;
    ws.load_entry(&entry);
    if let Some(run) = ws.last_run.as_mut() {
        run.source = RunSource::Written;
        run.finish_reason = "stop".to_string();
        run.elapsed_ms = Some(22_640);
    }
    let checklist = app.library.checklist_items();
    for (section, item) in checklist.iter().filter(|(s, _)| section_mode(s) == Some(Mode::R2v)).take(3) {
        ws.ticks.insert(tick_key(section, item));
    }
    app.last_timings = Some(timings);
}

/// Ctrl+Enter write (or send the fix from the fix field), Esc cancel (unless it closes a popup or
/// leaves a text field, see `AppState::esc_free`), Ctrl+S save .txt, Ctrl+Shift+C copy the
/// prompt (Ctrl+C too when nothing is being edited).
fn read_shortcuts(app: &AppState, ctx: &egui::Context, acts: &mut Actions) {
    let esc_cancels = app.write.job.is_some() && app.esc_free;
    let fix_focused = ctx.memory(|m| m.has_focus(egui::Id::new(FIX_FIELD)));
    let nothing_focused = ctx.memory(|m| m.focused().is_none());
    let label_selected = egui::text_selection::LabelSelectionState::load(ctx).has_selection();
    ctx.input_mut(|i| {
        if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter)) {
            if fix_focused {
                acts.send_fix = true;
            } else {
                acts.submit = true;
            }
        }
        if esc_cancels && i.consume_key(Modifiers::NONE, Key::Escape) {
            acts.cancel = true;
        }
        if i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::S)) {
            acts.save_txt = true;
        }
        // Ctrl+C arrives as a Copy event (also with Shift held), not as a key press.
        let copy = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
        if copy && i.modifiers.command && i.modifiers.shift {
            i.events.retain(|e| !matches!(e, egui::Event::Copy));
            acts.copy_prompt = true;
            acts.copy_by_key = true;
        } else if copy && nothing_focused && !label_selected {
            acts.copy_prompt = true;
            acts.copy_by_key = true;
        }
    });
}

fn apply_actions(app: &mut AppState, ctx: &egui::Context, acts: Actions, view: &View) {
    if acts.cancel {
        cancel_job(app);
    }
    if let Some(id) = acts.cancel_background {
        app.cancel(id);
    }
    if acts.submit && !view.running {
        if view.can_write {
            submit(app);
        } else if let Some(hint) = &view.hint {
            app.toast(ToastKind::Warn, hint.text.clone());
        }
    }
    if acts.send_fix && !view.running {
        send_fix(app, view);
    }
    if acts.start_server {
        app.send(Command::StartServer);
    }
    if acts.stop_server {
        app.send(Command::StopServer);
    }
    if acts.clear_prompt && !view.running {
        app.write.clear_prompt();
    }
    if acts.undo && !view.running {
        app.write.apply_undo();
    } else if acts.dismiss_undo {
        app.write.undo = None;
    }
    if acts.show_earlier && !view.running {
        for (kind, text) in app.write.show_earlier() {
            app.toast(kind, text);
        }
    } else if acts.dismiss_earlier {
        app.write.earlier = None;
    }
    if acts.copy_prompt && !app.write.prompt.trim().is_empty() {
        ctx.copy_text(app.write.prompt.clone());
        if acts.copy_by_key {
            app.toast(ToastKind::Info, "Prompt copied to the clipboard");
        }
    }
    if acts.save_txt && !view.running {
        save_txt(app);
    }
    if let Some(ws) = acts.open {
        app.workspace = ws;
    }
    let pointer_down = ctx.input(|i| i.pointer.any_down());
    if acts.save_config || (app.write.config_dirty && !pointer_down) {
        app.write.config_dirty = false;
        app.save_config();
    }
    update_activity(app);
}

/// Status bar text while one of our jobs runs (elapsed time included), cleared when they end.
/// The app calls it every frame, so it keeps counting on other workspaces. A job that only
/// waits behind another one (a chat answer) leaves that one's text in place.
pub fn update_activity(app: &mut AppState) {
    let now = app.time;
    let ws = &mut app.write;
    let ours = app.activity.is_none() || app.activity == ws.activity;
    let text = match &ws.job {
        Some(job) if ours || job.phase != phase::QUEUED => Some(job.status_text(now)),
        Some(_) => return,
        None if !ws.background.is_empty() && ours => Some("Writing an earlier prompt...".to_string()),
        None => None,
    };
    match text {
        Some(text) => {
            app.activity = Some(text.clone());
            ws.activity = Some(text);
        }
        None => {
            if ws.activity.take().is_some() && ours {
                app.activity = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

/// Send the request: the form, or the edited text with the facts it states (see
/// [`WriteState::text_lint_context`]). The pictures ticked "Send to Bonsai" go along either way.
fn submit(app: &mut AppState) {
    if app.write.job.is_some() {
        return;
    }
    let params = app.config.writer_params.clone();
    let ws = &app.write;
    let mode = ws.spec.mode;
    let built = if ws.edited {
        let text = ws.text.trim();
        if text.is_empty() {
            Err("The request text is empty".to_string())
        } else {
            ws.spec
                .images()
                .map(|images| {
                    let mut job = WriteJob::from_text(mode, text, params);
                    job.lint_ctx = ws.text_lint_context();
                    job.images = images;
                    let sent = SentRequest {
                        mode,
                        text: text.to_string(),
                        spec: Some(ws.spec.clone()),
                        form_text: false,
                        lint_ctx: job.lint_ctx.clone(),
                    };
                    (job, sent)
                })
                .map_err(|e| format!("Could not send the pictures: {e:#}"))
        }
    } else {
        WriteJob::from_spec(&ws.spec, params)
            .map(|job| {
                let sent = SentRequest {
                    mode,
                    text: job.request.clone(),
                    spec: Some(ws.spec.clone()),
                    form_text: true,
                    lint_ctx: job.lint_ctx.clone(),
                };
                (job, sent)
            })
            .map_err(|e| format!("Could not send the pictures: {e:#}"))
    };
    match built {
        Err(message) => app.toast(ToastKind::Error, message),
        Ok((mut job, sent)) => {
            job.save_history = app.config.save_history;
            start_job(app, job, Some(sent));
            if app.config.last_mode != mode {
                app.config.last_mode = mode;
                app.save_config();
            }
        }
    }
}

/// Send the fix as a follow-up on the current prompt.
fn send_fix(app: &mut AppState, view: &View) {
    let ws = &app.write;
    let fix = ws.fix_text.trim().to_string();
    if ws.job.is_some() || fix.is_empty() || ws.prompt.trim().is_empty() {
        return;
    }
    if !view.can_run {
        if let Some(hint) = &view.hint {
            app.toast(ToastKind::Warn, hint.text.clone());
        }
        return;
    }
    let mut job = fix_job(ws, fix, app.config.writer_params.clone());
    job.save_history = app.config.save_history;
    let sent = ws.last_sent.clone();
    start_job(app, job, sent);
}

/// The follow-up job for `fix` on the prompt on screen: checked against the prompt's own facts
/// and in its mode (an earlier fix may have changed the length or the prompt kind), after the
/// request it was written from with that request's pictures (none for a pasted prompt).
fn fix_job(ws: &WriteState, fix: String, params: GenParams) -> WriteJob {
    let ctx = ws.prompt_context();
    let sent = ws.last_sent.as_ref();
    let mut job = WriteJob::from_text(ctx.mode, fix.clone(), params);
    job.lint_ctx = ctx;
    let mut previous_request = sent.map(|s| s.text.clone()).unwrap_or_default();
    if let Some(sent) = sent {
        // The pictures of the first turn go along, so llama-server can reuse its cache. Pictures
        // that can no longer be read are left out and the earlier request says so.
        if let Some(spec) = sent.spec.as_ref().filter(|s| s.sent_image_count() > 0) {
            let (read, missing) = spec.readable_images();
            if !missing.is_empty() {
                let names: Vec<String> = read.iter().map(|(name, _)| name.clone()).collect();
                previous_request = bh3::request::with_attached_line(&previous_request, &names);
            }
            job.images = read.into_iter().map(|(_, image)| image).collect();
        }
        job.spec = sent.spec.clone().filter(|_| sent.form_text);
    }
    job.followup =
        Some(FollowUp { previous_request, previous_output: ws.prompt.clone(), fix, parent_id: ws.parent_id.clone() });
    job
}

fn start_job(app: &mut AppState, job: WriteJob, sent: Option<SentRequest>) {
    let id = app.submit(JobKind::Write(job.clone()));
    let now = app.time;
    let ws = &mut app.write;
    let previous = PromptSnapshot {
        prompt: std::mem::take(&mut ws.prompt),
        reasoning: std::mem::take(&mut ws.reasoning),
        lint: ws.lint.take(),
        prompt_ctx: ws.prompt_ctx.take(),
        edited: std::mem::take(&mut ws.prompt_edited),
    };
    ws.jump_to_line = None;
    // Undo would put back a state from before this job.
    ws.undo = None;
    ws.job = Some(RunningJob {
        id,
        job,
        sent,
        phase: "Sending...".to_string(),
        started: now,
        cancel_requested: false,
        previous,
    });
    update_activity(app);
}

fn cancel_job(app: &mut AppState) {
    let id = match app.write.job.as_mut() {
        Some(job) if !job.cancel_requested => {
            job.cancel_requested = true;
            job.phase = phase::CANCELLING.to_string();
            job.id
        }
        _ => return,
    };
    app.cancel(id);
}

fn save_txt(app: &mut AppState) {
    if app.write.prompt.trim().is_empty() {
        return;
    }
    let name = format!("{}.txt", util::slug(&app.write.spec.title(), 48));
    let Some(path) = util::pick_save_txt("Save the prompt", &name) else { return };
    match util::write_text_for_windows(&path, &app.write.prompt) {
        Ok(()) => app.toast(ToastKind::Ok, format!("Saved {}", file_name(&path))),
        Err(e) => app.toast(ToastKind::Error, format!("Could not save {}: {e:#}", path.display())),
    }
}

// ---------------------------------------------------------------------------------------------
// Left toolbar
// ---------------------------------------------------------------------------------------------

fn side_frame() -> egui::Frame {
    egui::Frame::new().fill(Palette::SIDE).inner_margin(Margin::symmetric(6, 6))
}

fn left_panel(ui: &mut Ui, ws: &mut WriteState, library: &Library) {
    ScrollArea::vertical().id_salt("write-left-scroll").auto_shrink([false, false]).show(ui, |ui| {
        widgets::panel(ui, "write-mode", "Mode", true, |ui| {
            let mut mode = ws.spec.mode;
            if widgets::enum_buttons(ui, &mut mode, &[(Mode::R2v, "r2v"), (Mode::F2v, "f2v")]) {
                ws.set_mode(mode);
            }
            ui.add_space(2.0);
            let about = match ws.spec.mode {
                Mode::R2v => "Reference images: up to 9 pictures give the people, the place, outfits and the look.",
                Mode::F2v => "First frame: Picture 1 is the exact first frame, Picture 2 an optional last frame.",
            };
            ui.label(RichText::new(about).small().color(Palette::TEXT_DIM));
        });

        let mode = ws.spec.mode;
        let templates: Vec<Template> = library.templates().into_iter().filter(|t| t.mode == mode).collect();
        let title = format!("Templates  ·  {}", templates.len());
        widgets::panel(ui, "write-templates", &title, true, |ui| {
            if templates.is_empty() {
                ui.label(RichText::new(format!("No {mode} templates in the Library.")).color(Palette::TEXT_DIM));
            }
            ui.with_layout(Layout::top_down_justified(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                // One line per template (Blender list rows); the full title is in the tooltip.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                for template in &templates {
                    let resp = ui.selectable_label(false, template_title(&template.title));
                    let resp = resp.on_hover_ui(|ui| {
                        ui.set_max_width(380.0);
                        ui.label(RichText::new(&template.title).strong());
                        ui.label(RichText::new(&template.text).monospace().size(12.0));
                        ui.label(
                            RichText::new("Click to put it into the Text tab, then fill in the <...> parts.")
                                .small()
                                .color(Palette::TEXT_DIM),
                        );
                    });
                    if resp.clicked() {
                        ws.use_template(template);
                    }
                }
            });
        });

        widgets::panel(ui, "write-tips", "Tips", true, |ui| {
            let image_tip = format!(
                "Describe pictures in words: sending an image costs ~{} tokens and ~{IMAGE_SECONDS} s each.",
                thousands(IMAGE_TOKENS as u64)
            );
            for tip in [
                "One continuous action per shot works best.",
                image_tip.as_str(),
                "Ref order = the Load Image order in ComfyUI.",
                "Thinking off is the tested default: valid prompts in ~10-25 s.",
            ] {
                bullet(ui, tip);
            }
        });
    });
}

/// Template titles without the long "(turn ... ON in ComfyUI)" part, capitalised.
fn template_title(title: &str) -> String {
    let short = match title.find(" (") {
        Some(i) if i >= 8 => &title[..i],
        _ => title,
    };
    capitalize(short.trim())
}

fn bullet(ui: &mut Ui, text: &str) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new("•").color(Palette::TEXT_DIM));
        ui.vertical(|ui| ui.label(RichText::new(text).color(Palette::TEXT)));
    });
}

// ---------------------------------------------------------------------------------------------
// Centre: Request + Prompt areas
// ---------------------------------------------------------------------------------------------

fn centre(ui: &mut Ui, ws: &mut WriteState, config: &AppConfig, view: &View, acts: &mut Actions) {
    let total_h = ui.available_height();
    let max_h = (total_h - 150.0).max(201.0);
    egui::TopBottomPanel::top("write-request-area")
        .resizable(true)
        .show_separator_line(false)
        .default_height((total_h * 0.5).round().clamp(200.0, max_h))
        .height_range(200.0..=max_h)
        .frame(egui::Frame::new().fill(Palette::REGION).outer_margin(Margin { bottom: 2, ..Margin::ZERO }))
        .show_inside(ui, |ui| request_area(ui, ws, view, acts));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(Palette::REGION))
        .show_inside(ui, |ui| prompt_area(ui, ws, config, view, acts));
}

fn request_area(ui: &mut Ui, ws: &mut WriteState, view: &View, acts: &mut Actions) {
    let warnings = ws.spec.warnings();
    request_header(ui, ws, &warnings);
    egui::TopBottomPanel::bottom("write-request-actions")
        .exact_height(ACTION_BAR_H)
        .frame(egui::Frame::new().fill(Palette::REGION).inner_margin(Margin::symmetric(10, 8)))
        .show_inside(ui, |ui| action_bar(ui, ws, view, acts));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(Palette::REGION).inner_margin(Margin { left: 8, right: 2, top: 6, bottom: 0 }))
        .show_inside(ui, |ui| match ws.tab {
            RequestTab::Builder => builder(ui, ws, &warnings),
            RequestTab::Text => text_tab(ui, ws),
        });
}

fn request_header(ui: &mut Ui, ws: &mut WriteState, warnings: &[String]) {
    // Own wording is sent and checked with what it states, not with the form.
    let summary = if ws.edited {
        format!("{} · own wording", context_line(&ws.text_lint_context()))
    } else {
        request_summary(&ws.spec)
    };
    let text_label = if ws.edited { "Text*" } else { "Text" };
    widgets::area_header(ui, "Request", |ui| {
        ui.allocate_ui_with_layout(egui::vec2(150.0, 22.0), Layout::left_to_right(Align::Center), |ui| {
            widgets::enum_buttons(ui, &mut ws.tab, &[(RequestTab::Builder, "Builder"), (RequestTab::Text, text_label)]);
        });
        if !warnings.is_empty() {
            let text = format!("{} {}", icons::WARN, warnings.len());
            widgets::badge(ui, &text, Palette::WARN).on_hover_text(warnings.join("\n"));
        }
        ui.add(egui::Label::new(RichText::new(summary).color(Palette::TEXT_DIM)).truncate());
    });
}

/// `r2v · 2 refs · 6 s (6.58 s) · one take`.
fn request_summary(spec: &RequestSpec) -> String {
    let used = spec.refs.len().min(request::max_refs(spec.mode));
    let pictures = match spec.mode {
        Mode::R2v => plural(used, "ref", "refs"),
        Mode::F2v if spec.has_last_frame() => "first + last frame".to_string(),
        Mode::F2v => "first frame".to_string(),
    };
    format!(
        "{} · {pictures} · {} s ({} s) · {}",
        spec.mode,
        spec.duration_s,
        lengths::format_real(spec.duration_s as f32),
        cuts_label(spec.cuts)
    )
}

/// The Write button (or Cancel + phase while a job runs) under the request.
fn action_bar(ui: &mut Ui, ws: &WriteState, view: &View, acts: &mut Actions) {
    ui.horizontal_centered(|ui| match &ws.job {
        None => {
            let resp = ui
                .allocate_ui(egui::vec2(ACTION_BUTTON_W, 30.0), |ui| {
                    widgets::primary_button(ui, "Write prompt", view.can_write)
                })
                .inner;
            let disabled_why = view.hint.as_ref().map_or("", |h| h.text.as_str()).to_string();
            if resp
                .on_hover_text("Write the H3 prompt with Bonsai (Ctrl+Enter)")
                .on_disabled_hover_text(disabled_why)
                .clicked()
            {
                acts.submit = true;
            }
            ui.label(RichText::new("Ctrl+Enter").small().color(Palette::TEXT_FAINT));
            ui.add_space(6.0);
            if let Some(undo) = &ws.undo {
                let tip = if undo.whole {
                    "Bring back the form, the request text and the prompt from before"
                } else {
                    "Bring back the prompt from before"
                };
                if ui.small_button("Undo").on_hover_text(tip).clicked() {
                    acts.undo = true;
                }
                if ui.small_button("✖").on_hover_text("Hide").clicked() {
                    acts.dismiss_undo = true;
                }
                ui.add(egui::Label::new(RichText::new(&undo.message).color(Palette::INFO)).truncate());
            } else if let Some(earlier) = &ws.earlier {
                let show = ui
                    .small_button("Show")
                    .on_hover_text("Put it into the Prompt area (Undo brings back the prompt there now)");
                if show.clicked() {
                    acts.show_earlier = true;
                }
                if ui.small_button("✖").on_hover_text("Dismiss it").clicked() {
                    acts.dismiss_earlier = true;
                }
                let text = format!("An earlier prompt finished (score {})", earlier.outcome.lint.score);
                ui.add(egui::Label::new(RichText::new(text).color(Palette::TEXT_DIM)).truncate());
            } else if !ws.background.is_empty() {
                ui.label(RichText::new("An earlier prompt is still being written").color(Palette::TEXT_DIM));
                if ui.small_button("Cancel it").on_hover_text("Cancel the earlier job").clicked() {
                    acts.cancel_background = ws.background.first().map(|j| j.id);
                }
            } else if let Some(hint) = &view.hint {
                if let Some(target) = hint.open {
                    if ui.small_button("Open").on_hover_text("Go to the Bonsai workspace").clicked() {
                        acts.open = Some(target);
                    }
                }
                ui.add(egui::Label::new(RichText::new(&hint.text).color(hint.color)).truncate());
            }
        }
        Some(job) => {
            let cancel =
                egui::Button::new(format!("{} Cancel (Esc)", icons::STOP)).min_size(egui::vec2(ACTION_BUTTON_W, 30.0));
            let resp = ui.add_enabled(!job.cancel_requested, cancel);
            if resp.on_hover_text("Stop writing (Esc)").clicked() {
                acts.cancel = true;
            }
            ui.add_space(4.0);
            ui.add(egui::Spinner::new().size(16.0).color(theme::accent(ui.ctx())));
            ui.label(RichText::new(job.status_text(view.now)).color(Palette::TEXT));
            let words = word_count(&ws.prompt);
            if words > 0 {
                ui.label(RichText::new(format!("· {} words", thousands(words as u64))).color(Palette::TEXT_DIM));
            }
        }
    });
}

// --- Builder ---------------------------------------------------------------------------------

fn builder(ui: &mut Ui, ws: &mut WriteState, warnings: &[String]) {
    ScrollArea::vertical().id_salt("write-builder-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(2.0);
        if ws.edited {
            edited_banner(ui, ws);
            ui.add_space(6.0);
        }
        references_panel(ui, &mut ws.spec);
        shot_panel(ui, &mut ws.spec);
        look_panel(ui, &mut ws.spec);
        dialogue_panel(ui, &mut ws.spec);
        widgets::panel(ui, "write-notes", "Notes", true, |ui| {
            ui.add(
                TextEdit::multiline(&mut ws.spec.notes)
                    .id_salt("write-notes")
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .hint_text("Anything else for Bonsai: light, mood, what to avoid"),
            );
        });
        if !warnings.is_empty() {
            let title = format!("Warnings  ·  {}", warnings.len());
            widgets::panel(ui, "write-warnings", &title, true, |ui| {
                for warning in warnings {
                    icon_line(ui, icons::WARN, Palette::WARN, warning);
                }
            });
        }
    });
}

/// Shown on the Builder tab when the Text tab has edits (they are what gets sent).
fn edited_banner(ui: &mut Ui, ws: &mut WriteState) {
    egui::Frame::new()
        .fill(Palette::WARN.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0_f32, Palette::WARN.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let pictures = match ws.spec.sent_image_count() {
                    0 => "",
                    _ => " The pictures ticked \"Send to Bonsai\" still go along.",
                };
                ui.label(
                    RichText::new(format!(
                        "{} The Text tab has your own wording: it is sent instead of this form.{pictures}",
                        icons::WARN
                    ))
                    .color(Palette::WARN),
                );
                let rebuild =
                    ui.button("Rebuild from form").on_hover_text("Drop the edits and build the text from this form");
                if rebuild.clicked() {
                    ws.rebuild_text();
                }
            });
        });
}

/// What a reference row does.
enum RefAction {
    Up(usize),
    Down(usize),
    Remove(usize),
}

fn references_panel(ui: &mut Ui, spec: &mut RequestSpec) {
    let mode = spec.mode;
    let max = request::max_refs(mode);
    let title = format!("References  ·  {}/{max}", spec.refs.len().min(max));
    widgets::panel(ui, "write-refs", &title, true, |ui| {
        let RequestSpec { refs, first_frame_pinned, .. } = spec;
        if mode == Mode::R2v {
            let mut pinned = *first_frame_pinned;
            let resp = ui
                .checkbox(&mut pinned, "Ref 1 is the exact first frame (First-frame pin)")
                .on_hover_text("Turn the First-frame pin group ON in ComfyUI too");
            if resp.changed() {
                *first_frame_pinned = pinned;
                if pinned {
                    if refs.is_empty() {
                        refs.push(RefImage::default());
                    }
                    refs[0].role = RefRole::FirstFrame;
                } else if let Some(first) = refs.first_mut().filter(|r| r.role == RefRole::FirstFrame) {
                    first.role = RefRole::Person;
                }
            }
            ui.add_space(4.0);
        }
        let count = refs.len();
        let mut action = None;
        for (i, r) in refs.iter_mut().enumerate() {
            if let Some(a) = ref_card(ui, mode, i, count, r, first_frame_pinned) {
                action = Some(a);
            }
            ui.add_space(4.0);
        }
        match action {
            Some(RefAction::Up(i)) if i > 0 && i < refs.len() => refs.swap(i, i - 1),
            Some(RefAction::Down(i)) if i + 1 < refs.len() => refs.swap(i, i + 1),
            Some(RefAction::Remove(i)) if i < refs.len() => {
                let removed = refs.remove(i);
                if let Some(path) = &removed.image_path {
                    ui.ctx().forget_image(&util::file_uri(path));
                }
            }
            _ => {}
        }
        ui.horizontal(|ui| {
            let full = refs.len() >= max;
            let label = match (mode, refs.len()) {
                (Mode::F2v, 0) => "Add first frame",
                (Mode::F2v, _) => "Add last frame",
                (Mode::R2v, _) => "Add reference",
            };
            let resp = ui.add_enabled(!full, egui::Button::new(format!("{} {label}", icons::ADD)));
            let limit = match mode {
                Mode::R2v => format!("r2v uses at most {} reference pictures", request::MAX_R2V_REFS),
                Mode::F2v => "f2v uses a first frame and an optional last frame".to_string(),
            };
            if resp.on_hover_text("Add a picture at the end of the list").on_disabled_hover_text(limit).clicked() {
                let role = new_ref_role(mode, refs);
                refs.push(RefImage { role, ..RefImage::default() });
            }
            ui.label(RichText::new("Order = the Load Image order in ComfyUI").small().color(Palette::TEXT_DIM));
        });
    });
}

/// Role of an added picture: the first / last frame in f2v; the place after a person in r2v.
fn new_ref_role(mode: Mode, refs: &[RefImage]) -> RefRole {
    match mode {
        Mode::F2v if refs.is_empty() => RefRole::FirstFrame,
        Mode::F2v => RefRole::LastFrame,
        Mode::R2v => {
            let has_person = refs.iter().any(|r| matches!(r.role, RefRole::Person | RefRole::FirstFrame));
            let has_place = refs.iter().any(|r| matches!(r.role, RefRole::Place | RefRole::PersonAndPlace));
            if has_person && !has_place {
                RefRole::Place
            } else {
                RefRole::Person
            }
        }
    }
}

/// One reference: thumbnail, name, role, "send to Bonsai", reorder/remove, description.
fn ref_card(
    ui: &mut Ui,
    mode: Mode,
    index: usize,
    count: usize,
    r: &mut RefImage,
    pinned: &mut bool,
) -> Option<RefAction> {
    let name = ref_name(mode, index);
    let unused = index >= request::max_refs(mode);
    let mut action = None;
    egui::Frame::new()
        .fill(CARD_FILL)
        .stroke(Stroke::new(1.0_f32, CARD_LINE))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::same(6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if unused {
                ui.multiply_opacity(0.55);
            }
            ui.horizontal_top(|ui| {
                thumb_slot(ui, r, &name);
                ui.vertical(|ui| {
                    let narrow = ui.available_width() < 470.0;
                    egui::Sides::new().height(22.0).show(
                        ui,
                        |ui| {
                            ui.allocate_ui_with_layout(
                                egui::vec2(62.0, 22.0),
                                Layout::left_to_right(Align::Center),
                                |ui| ui.label(RichText::new(&name).strong()),
                            );
                            role_combo(ui, mode, index, r, pinned, if narrow { 124.0 } else { 196.0 });
                            send_checkbox(ui, r, narrow);
                        },
                        |ui| {
                            let remove = widgets::icon_button(ui, icons::TRASH, "Remove this picture");
                            if remove.clicked() {
                                action = Some(RefAction::Remove(index));
                            }
                            let down = ui
                                .add_enabled_ui(index + 1 < count, |ui| {
                                    widgets::icon_button(ui, icons::DOWN, "Move down (later in the Load Image order)")
                                })
                                .inner;
                            if down.clicked() {
                                action = Some(RefAction::Down(index));
                            }
                            let up = ui
                                .add_enabled_ui(index > 0, |ui| {
                                    widgets::icon_button(ui, icons::UP, "Move up (earlier in the Load Image order)")
                                })
                                .inner;
                            if up.clicked() {
                                action = Some(RefAction::Up(index));
                            }
                        },
                    );
                    ui.add(
                        TextEdit::multiline(&mut r.description)
                            .id_salt(("write-ref-desc", index))
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .hint_text(role_hint(mode, index, r.role)),
                    );
                    if unused {
                        let text = match mode {
                            Mode::R2v => "Not used: r2v takes Ref 1 to Ref 9",
                            Mode::F2v => "Not used in f2v: only Picture 1 and Picture 2 are used",
                        };
                        ui.label(RichText::new(text).small().color(Palette::WARN));
                    }
                });
            });
        });
    action
}

fn role_combo(ui: &mut Ui, mode: Mode, index: usize, r: &mut RefImage, pinned: &mut bool, width: f32) {
    let before = r.role;
    egui::ComboBox::from_id_salt(("write-ref-role", index))
        .width(width)
        .selected_text(r.role.label())
        .show_ui(ui, |ui| {
            for &role in RefRole::all() {
                let offered = role.allowed_in(mode) && !(mode == Mode::R2v && role == RefRole::FirstFrame && index > 0);
                if offered || role == r.role {
                    ui.selectable_value(&mut r.role, role, role.label());
                }
            }
        })
        .response
        .on_hover_text("What this picture gives the video");
    // In r2v, "Exact first frame" on Ref 1 and the First-frame pin go together.
    if mode == Mode::R2v && index == 0 && r.role != before {
        *pinned = r.role == RefRole::FirstFrame;
    }
}

fn send_checkbox(ui: &mut Ui, r: &mut RefImage, narrow: bool) {
    let label = if narrow { "Send" } else { "Send to Bonsai" };
    let enabled = r.image_path.is_some() || r.send_image;
    let resp = ui.add_enabled(enabled, egui::Checkbox::new(&mut r.send_image, label));
    resp.on_hover_text(format!(
        "Send the picture itself to Bonsai's vision input: ~{} tokens and ~{IMAGE_SECONDS} s each.\n\
         Describing it in words is usually enough.",
        thousands(IMAGE_TOKENS as u64)
    ))
    .on_disabled_hover_text("Pick an image first (click the slot on the left)");
    if r.send_image && !narrow {
        ui.label(RichText::new(format!("+{} tokens", thousands(IMAGE_TOKENS as u64))).small().color(Palette::WARN));
    }
}

/// 56x56 picture slot: the thumbnail, or "Pick image..." when there is none. Click to choose a
/// file, right-click for more.
fn thumb_slot(ui: &mut Ui, r: &mut RefImage, name: &str) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(THUMB, THUMB), Sense::click());
    let radius = CornerRadius::same(4);
    let inner = rect.size() - egui::vec2(4.0, 4.0);
    ui.painter().rect_filled(rect, radius, Palette::INPUT);
    let existing = r.image_path.clone();
    let exists = existing.as_deref().is_some_and(Path::is_file);
    let tooltip = match &existing {
        Some(path) if exists => {
            let image =
                egui::Image::new(util::file_uri(path)).fit_to_exact_size(inner).corner_radius(CornerRadius::same(3));
            let size = image.load_and_calc_size(ui, inner).unwrap_or(inner);
            image.paint_at(ui, Rect::from_center_size(rect.center(), size));
            format!("{}\nClick to choose another picture · right-click for more", path.display())
        }
        Some(path) => {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                icons::WARN,
                FontId::proportional(20.0),
                Palette::WARN,
            );
            format!("File not found:\n{}\nClick to choose the picture again", path.display())
        }
        None => {
            let color = if resp.hovered() { Palette::TEXT } else { Palette::TEXT_DIM };
            for (line, dy) in [("Pick", -7.0), ("image...", 7.0)] {
                let pos = rect.center() + egui::vec2(0.0, dy);
                ui.painter().text(pos, Align2::CENTER_CENTER, line, FontId::proportional(11.5), color);
            }
            "Pick image... (optional)\nA thumbnail for you: the picture is sent to Bonsai only with \"Send to Bonsai\""
                .to_string()
        }
    };
    let stroke_color = if resp.hovered() { Palette::BLUE_HOVER } else { Palette::OUTLINE };
    ui.painter().rect_stroke(rect, radius, Stroke::new(1.0_f32, stroke_color), egui::StrokeKind::Inside);

    let mut pick = resp.clicked();
    let mut remove = false;
    if existing.is_some() && ui.rect_contains_pointer(rect) {
        let x_rect = Rect::from_min_size(rect.right_top() + egui::vec2(-17.0, 1.0), egui::vec2(16.0, 16.0));
        let x = ui.interact(x_rect, ui.id().with(("write-thumb-remove", name)), Sense::click());
        let fill = if x.hovered() { Palette::ERR } else { Color32::from_black_alpha(170) };
        ui.painter().rect_filled(x_rect, CornerRadius::same(3), fill);
        ui.painter().text(x_rect.center(), Align2::CENTER_CENTER, "✖", FontId::proportional(10.0), Color32::WHITE);
        if x.on_hover_text("Remove the picture").clicked() {
            remove = true;
            pick = false;
        }
    }
    resp.context_menu(|ui| {
        if ui.button("Choose picture...").clicked() {
            pick = true;
            ui.close_menu();
        }
        if ui.add_enabled(existing.is_some(), egui::Button::new("Remove picture")).clicked() {
            remove = true;
            ui.close_menu();
        }
        if let Some(path) = existing.as_ref().filter(|_| exists) {
            if ui.button("Show in Explorer").clicked() {
                if let Err(e) = util::reveal_in_explorer(path) {
                    eprintln!("reveal {}: {e:#}", path.display());
                }
                ui.close_menu();
            }
        }
    });
    resp.on_hover_text(tooltip);

    if pick {
        if let Some(path) = util::pick_image(&format!("Picture for {name}")) {
            if let Some(old) = &existing {
                ui.ctx().forget_image(&util::file_uri(old));
            }
            r.image_path = Some(path);
        }
    }
    if remove {
        if let Some(old) = &existing {
            ui.ctx().forget_image(&util::file_uri(old));
        }
        r.image_path = None;
        r.send_image = false;
    }
}

fn shot_panel(ui: &mut Ui, spec: &mut RequestSpec) {
    widgets::panel(ui, "write-shot", "Shot", true, |ui| {
        widgets::prop_row(ui, "Duration", |ui| {
            let resp = egui::ComboBox::from_id_salt("write-duration")
                .width(150.0)
                .selected_text(duration_label(spec.duration_s))
                .show_ui(ui, |ui| {
                    for d in lengths::DURATIONS {
                        ui.selectable_value(&mut spec.duration_s, d, duration_label(d));
                    }
                })
                .response;
            resp.on_hover_text(
                "H3 renders 24 fps on a 17k+5 frame grid, so the clip is a little longer than asked.\n\
                 The prompt uses the real length.",
            );
            let frames = lengths::frames_for_seconds(spec.duration_s as f32);
            ui.label(RichText::new(format!("{frames} frames at 24 fps")).color(Palette::TEXT_DIM));
        });
        widgets::prop_row(ui, "Cuts", |ui| {
            let suggested = lengths::suggested_max_cuts(spec.duration_s as f32);
            ui.add(
                egui::DragValue::new(&mut spec.cuts)
                    .range(0..=suggested + 1)
                    .speed(0.05)
                    .custom_formatter(|v, _| cuts_label(v.round().max(0.0) as u32))
                    .custom_parser(parse_cuts),
            )
            .on_hover_text("0 = one continuous take. About one cut per 3 s works best.");
            let text = format!("up to {suggested} for {} s", spec.duration_s);
            ui.label(RichText::new(text).color(Palette::TEXT_DIM));
        });
        widgets::prop_row(ui, "Action", |ui| {
            ui.add(
                TextEdit::multiline(&mut spec.action)
                    .id_salt("write-action")
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("What happens: one continuous action per shot works best"),
            );
        });
    });
}

fn look_panel(ui: &mut Ui, spec: &mut RequestSpec) {
    widgets::panel(ui, "write-look", "Look", true, |ui| {
        widgets::prop_row(ui, "Style", |ui| {
            egui::ComboBox::from_id_salt("write-style").width(170.0).selected_text(spec.style.label()).show_ui(
                ui,
                |ui| {
                    for &style in StylePreset::all() {
                        let resp = ui.selectable_value(&mut spec.style, style, style.label());
                        if !style.words().is_empty() {
                            resp.on_hover_text(style.words());
                        }
                    }
                },
            );
            if spec.style == StylePreset::Custom {
                ui.add(
                    TextEdit::singleline(&mut spec.style_custom)
                        .id_salt("write-style-custom")
                        .desired_width(f32::INFINITY)
                        .hint_text("your look: colours, light, grain, lens"),
                );
            } else {
                ui.add(egui::Label::new(RichText::new(spec.style.words()).color(Palette::TEXT_DIM)).truncate());
            }
        });
        widgets::prop_row(ui, "Camera", |ui| {
            egui::ComboBox::from_id_salt("write-camera").width(170.0).selected_text(spec.camera.label()).show_ui(
                ui,
                |ui| {
                    for &camera in CameraPreset::all() {
                        let resp = ui.selectable_value(&mut spec.camera, camera, camera.label());
                        if !camera.words().is_empty() {
                            resp.on_hover_text(camera.words());
                        }
                    }
                },
            );
            match spec.camera {
                CameraPreset::Custom => {
                    ui.add(
                        TextEdit::singleline(&mut spec.camera_custom)
                            .id_salt("write-camera-custom")
                            .desired_width(f32::INFINITY)
                            .hint_text("how the camera moves"),
                    );
                }
                CameraPreset::Auto => {
                    ui.label(RichText::new("Bonsai chooses").color(Palette::TEXT_DIM));
                }
                preset => {
                    ui.add(egui::Label::new(RichText::new(preset.words()).color(Palette::TEXT_DIM)).truncate());
                }
            }
        });
        let rows: [(&str, &mut String, &str, &str); 3] = [
            (
                "Place",
                &mut spec.place,
                "write-place",
                "where it happens, light, time of day (when no picture gives it)",
            ),
            ("Outfit", &mut spec.outfit, "write-outfit", "empty = keep the outfit of the pictures"),
            (
                "Extra people",
                &mut spec.extra_people,
                "write-extra-people",
                "people without a photo, e.g. a young man with short dark hair, blue shirt",
            ),
        ];
        for (label, value, id, hint) in rows {
            widgets::prop_row(ui, label, |ui| {
                ui.add(
                    TextEdit::multiline(value).id_salt(id).desired_rows(1).desired_width(f32::INFINITY).hint_text(hint),
                );
            });
        }
    });
}

fn dialogue_panel(ui: &mut Ui, spec: &mut RequestSpec) {
    let title = match spec.dialogue.len() {
        0 => "Dialogue".to_string(),
        n => format!("Dialogue  ·  {n}"),
    };
    widgets::panel(ui, "write-dialogue", &title, true, |ui| {
        let mut remove = None;
        for (i, line) in spec.dialogue.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut line.speaker)
                        .id_salt(("write-dialogue-speaker", i))
                        .desired_width(110.0)
                        .hint_text("she / the man"),
                );
                let width = (ui.available_width() - 30.0).max(80.0);
                ui.add(
                    TextEdit::singleline(&mut line.words)
                        .id_salt(("write-dialogue-words", i))
                        .desired_width(width)
                        .hint_text("the exact words"),
                );
                if widgets::icon_button(ui, icons::TRASH, "Remove this line").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            spec.dialogue.remove(i);
        }
        ui.horizontal(|ui| {
            let add = ui.button(format!("{} Add line", icons::ADD)).on_hover_text("Add a line of dialogue");
            if add.clicked() {
                spec.dialogue.push(DialogueLine::default());
            }
            ui.label(
                RichText::new("Exact words are kept; Bonsai adds the speaker ids and <d> tags")
                    .small()
                    .color(Palette::TEXT_DIM),
            );
        });
    });
}

// --- Text tab --------------------------------------------------------------------------------

fn text_tab(ui: &mut Ui, ws: &mut WriteState) {
    let mut rebuild = false;
    egui::Sides::new().height(24.0).show(
        ui,
        |ui| {
            if ws.edited {
                ui.label(
                    RichText::new("Your own wording: this text is sent instead of the form.").color(Palette::WARN),
                );
                let button =
                    ui.button("Rebuild from form").on_hover_text("Drop the edits and build the text from the form");
                rebuild = button.clicked();
            } else {
                ui.label(
                    RichText::new("Built from the form. Edit it to send your own wording.").color(Palette::TEXT_DIM),
                );
            }
        },
        |ui| {
            widgets::copy_button(ui, &ws.text, "Copy");
        },
    );
    if rebuild {
        ws.rebuild_text();
    }
    ui.add_space(4.0);
    let view_h = ui.available_height();
    ScrollArea::vertical().id_salt("write-text-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let rows = rows_for(ui, view_h, &TextStyle::Monospace);
        if widgets::code_view(ui, "write-request-text", &mut ws.text, rows, true).changed() {
            ws.edited = true;
            if ws.text.trim().is_empty() {
                // What is typed now is the user's own request, not an edit of the form's.
                ws.text_from_form = false;
                ws.text_ctx = None;
            }
        }
    });
}

// --- Prompt area -----------------------------------------------------------------------------

fn prompt_area(ui: &mut Ui, ws: &mut WriteState, config: &AppConfig, view: &View, acts: &mut Actions) {
    prompt_header(ui, ws, view, acts);
    egui::Frame::new().inner_margin(Margin { left: 8, right: 2, top: 6, bottom: 6 }).show(ui, |ui| {
        if config.show_reasoning && !ws.reasoning.trim().is_empty() {
            reasoning_section(ui, ws, view.running);
        }
        prompt_editor(ui, ws, config.monospace_prompts, view.running);
    });
}

fn prompt_header(ui: &mut Ui, ws: &WriteState, view: &View, acts: &mut Actions) {
    let has_prompt = !ws.prompt.trim().is_empty();
    let idle = !view.running;
    let words = match &ws.lint {
        Some(report) if idle => report.word_count,
        _ => word_count(&ws.prompt),
    };
    widgets::area_header(ui, "Prompt", |ui| {
        let clear =
            ui.add_enabled_ui(has_prompt && idle, |ui| widgets::icon_button(ui, icons::TRASH, "Clear the prompt"));
        if clear.inner.clicked() {
            acts.clear_prompt = true;
        }
        let save = ui
            .add_enabled(has_prompt && idle, egui::Button::new(format!("{} Save .txt", icons::SAVE)))
            .on_hover_text("Save the prompt as a .txt file for ComfyUI (Ctrl+S)");
        if save.clicked() {
            acts.save_txt = true;
        }
        let copy = ui
            .add_enabled(has_prompt, egui::Button::new(format!("{} Copy", icons::COPY)))
            .on_hover_text("Copy the prompt to the clipboard (Ctrl+Shift+C)");
        if copy.clicked() {
            acts.copy_prompt = true;
        }
        ui.add_space(6.0);
        if let Some(report) = ws.lint.as_ref().filter(|_| idle) {
            let text = format!("score {}", report.score);
            widgets::badge(ui, &text, score_color(report)).on_hover_text(report.summary());
        }
        if has_prompt {
            ui.label(RichText::new(format!("{} words", thousands(words as u64))).color(Palette::TEXT_DIM));
        }
    });
}

fn reasoning_section(ui: &mut Ui, ws: &WriteState, running: bool) {
    let title = format!("Reasoning  ·  {} words", thousands(word_count(&ws.reasoning) as u64));
    egui::CollapsingHeader::new(RichText::new(title).color(Palette::TEXT_DIM))
        .id_salt("write-reasoning")
        .default_open(true)
        .show(ui, |ui| {
            ScrollArea::vertical()
                .id_salt("write-reasoning-scroll")
                .max_height(150.0)
                .auto_shrink([false, true])
                .stick_to_bottom(running)
                .show(ui, |ui| {
                    ui.label(RichText::new(&ws.reasoning).size(12.5).color(Palette::TEXT_DIM));
                });
            widgets::copy_button(ui, &ws.reasoning, "Copy reasoning");
        });
    ui.add_space(4.0);
}

/// The prompt: streams read-only while a job runs, editable (and re-checked on edit) otherwise.
fn prompt_editor(ui: &mut Ui, ws: &mut WriteState, monospace: bool, running: bool) {
    let id = egui::Id::new(PROMPT_EDITOR);
    // Pasting (or typing) over the whole prompt puts in another prompt, as an empty editor does.
    let was_empty = ws.prompt.trim().is_empty();
    let replacing = !running
        && !was_empty
        && ui.memory(|m| m.has_focus(id))
        && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_) | egui::Event::Text(_))))
        && TextEdit::load_state(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range())
            .is_some_and(|r| selects_all(r.primary.index, r.secondary.index, &ws.prompt));
    let jump = ws.jump_to_line.take().and_then(|line| line_char_range(&ws.prompt, line));
    if let Some((start, end)) = jump {
        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(start),
            egui::text::CCursor::new(end),
        )));
        state.store(ui.ctx(), id);
        ui.memory_mut(|m| m.request_focus(id));
    }
    let style = if monospace { TextStyle::Monospace } else { TextStyle::Body };
    let view_h = ui.available_height();
    ScrollArea::vertical().id_salt("write-prompt-scroll").auto_shrink([false, false]).stick_to_bottom(running).show(
        ui,
        |ui| {
            let rows = rows_for(ui, view_h, &style);
            let hint = if running {
                "Waiting for Bonsai..."
            } else {
                "The prompt appears here. You can also paste an H3 prompt to check it."
            };
            let output = TextEdit::multiline(&mut ws.prompt)
                .id(id)
                .font(style.clone())
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .lock_focus(true)
                .interactive(!running)
                .background_color(Palette::INPUT)
                .hint_text(hint)
                .show(ui);
            if output.response.changed() {
                if replacing || was_empty || ws.prompt.trim().is_empty() {
                    ws.forget_prompt_source();
                }
                // Hand edits and a pasted prompt are saved nowhere: a new answer over them offers Undo.
                ws.prompt_edited = !ws.prompt.trim().is_empty();
                ws.relint();
            }
            if let Some((start, _)) = jump {
                let cursor = output.galley.pos_from_ccursor(egui::text::CCursor::new(start));
                ui.scroll_to_rect(cursor.translate(output.galley_pos.to_vec2()), Some(Align::Center));
            }
        },
    );
}

// ---------------------------------------------------------------------------------------------
// Right N-panel
// ---------------------------------------------------------------------------------------------

struct RightPanel<'a> {
    ws: &'a mut WriteState,
    config: &'a mut AppConfig,
    status: &'a StatusSnapshot,
    library: &'a Library,
    view: &'a View,
    acts: &'a mut Actions,
}

impl RightPanel<'_> {
    fn show(&mut self, ui: &mut Ui) {
        ScrollArea::vertical().id_salt("write-right-scroll").auto_shrink([false, false]).show(ui, |ui| {
            self.bonsai(ui);
            self.generation(ui);
            self.check(ui);
            self.fix(ui);
            self.checklist(ui);
            self.last_run(ui);
        });
    }

    fn bonsai(&mut self, ui: &mut Ui) {
        let status = self.status;
        let acts = &mut *self.acts;
        let config = &mut *self.config;
        widgets::panel(ui, "write-bonsai", "Bonsai", true, |ui| {
            let state = &status.state;
            egui::Sides::new().show(
                ui,
                |ui| {
                    widgets::status_dot(ui, state_color(state), &state_text(state));
                },
                |ui| match state {
                    ServerState::Stopped | ServerState::Failed(_) => {
                        let start =
                            ui.add_enabled(status.install_ok, egui::Button::new(format!("{} Start", icons::PLAY)));
                        if start
                            .on_hover_text("Start Bonsai now (~7.7 GB of VRAM: close ComfyUI first)")
                            .on_disabled_hover_text("Choose the Bonsai folder first (Bonsai workspace)")
                            .clicked()
                        {
                            acts.start_server = true;
                        }
                    }
                    ServerState::Stopping => {
                        ui.add_enabled(false, egui::Button::new(format!("{} Stop", icons::STOP)));
                    }
                    _ => {
                        let stop = ui
                            .button(format!("{} Stop", icons::STOP))
                            .on_hover_text("Stop Bonsai and free the GPU for ComfyUI (cancels running jobs)");
                        if stop.clicked() {
                            acts.stop_server = true;
                        }
                    }
                },
            );
            if let ServerState::Failed(message) = state {
                ui.label(RichText::new(message).small().color(Palette::ERR));
            }
            let (model, model_tip) = model_text(status);
            widgets::prop_row(ui, "Model", |ui| {
                ui.add(egui::Label::new(model).truncate()).on_hover_text(model_tip);
            });
            if let Some(gpu) = &status.gpu {
                widgets::prop_row(ui, "VRAM", |ui| {
                    let text = format!("{} / {}", widgets::fmt_mb(gpu.mem_used_mb), widgets::fmt_mb(gpu.mem_total_mb));
                    ui.label(text).on_hover_text(gpu.name.as_str());
                });
            }
            if let Some(secs) = status.auto_stop_in_secs {
                widgets::prop_row(ui, "Auto-stop", |ui| {
                    ui.label(RichText::new(format!("in {}", widgets::fmt_secs(secs))).color(Palette::TEXT_DIM))
                        .on_hover_text("Bonsai stops after the idle minutes set in the Bonsai workspace");
                });
            }
            ui.add_space(2.0);
            let mut auto = config.auto_start;
            let resp = ui
                .checkbox(&mut auto, "Start automatically when needed")
                .on_hover_text("Start Bonsai when a prompt is requested (it needs ~7.7 GB of VRAM)");
            if resp.changed() {
                config.auto_start = auto;
                acts.save_config = true;
            }
        });
    }

    fn generation(&mut self, ui: &mut Ui) {
        let params = &mut self.config.writer_params;
        let dirty = &mut self.ws.config_dirty;
        widgets::panel(ui, "write-generation", "Generation", true, |ui| {
            ui.label(RichText::new("Thinking").color(Palette::TEXT_DIM));
            let options = ThinkingMode::all().map(|m| (m, m.label()));
            if widgets::enum_buttons(ui, &mut params.thinking, &options) {
                *dirty = true;
            }
            let hint_color = if params.thinking == ThinkingMode::Off { Palette::TEXT_DIM } else { Palette::WARN };
            ui.label(RichText::new(params.thinking.hint()).small().color(hint_color));
            ui.add_space(4.0);
            widgets::prop_row(ui, "Temperature", |ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 54.0).max(60.0);
                let slider = egui::Slider::new(&mut params.temperature, 0.0..=1.5).step_by(0.05).fixed_decimals(2);
                if ui.add(slider).on_hover_text("0.4 is the tested value for prompt writing").changed() {
                    *dirty = true;
                }
            });
            widgets::prop_row(ui, "Max tokens", |ui| {
                let field = egui::DragValue::new(&mut params.max_tokens).range(64..=16_384).speed(16);
                let resp = ui
                    .add_sized([ui.available_width(), 20.0], field)
                    .on_hover_text("Most new tokens (reasoning + answer). A prompt needs ~600-900.");
                if resp.changed() {
                    *dirty = true;
                }
            });
            egui::CollapsingHeader::new("Advanced").id_salt("write-generation-advanced").default_open(false).show(
                ui,
                |ui| {
                    widgets::prop_row(ui, "Top-p", |ui| {
                        let field =
                            egui::DragValue::new(&mut params.top_p).range(0.0..=1.0).speed(0.01).fixed_decimals(2);
                        if ui.add_sized([ui.available_width(), 20.0], field).changed() {
                            *dirty = true;
                        }
                    });
                    widgets::prop_row(ui, "Top-k", |ui| {
                        let field = egui::DragValue::new(&mut params.top_k).range(0..=200).speed(0.2);
                        if ui.add_sized([ui.available_width(), 20.0], field).changed() {
                            *dirty = true;
                        }
                    });
                    widgets::prop_row(ui, "Min-p", |ui| {
                        let field =
                            egui::DragValue::new(&mut params.min_p).range(0.0..=1.0).speed(0.005).fixed_decimals(2);
                        if ui.add_sized([ui.available_width(), 20.0], field).changed() {
                            *dirty = true;
                        }
                    });
                    widgets::prop_row(ui, "Seed", |ui| {
                        let mut fixed = params.seed.is_some();
                        if ui
                            .checkbox(&mut fixed, "Fixed")
                            .on_hover_text("The same seed gives the same answer")
                            .changed()
                        {
                            params.seed = fixed.then_some(params.seed.unwrap_or(42));
                            *dirty = true;
                        }
                        match params.seed.as_mut() {
                            Some(seed) => {
                                let field = egui::DragValue::new(seed).range(0..=i64::from(u32::MAX)).speed(1.0);
                                if ui.add_sized([ui.available_width(), 20.0], field).changed() {
                                    *dirty = true;
                                }
                            }
                            None => {
                                ui.label(RichText::new("random").color(Palette::TEXT_DIM));
                            }
                        }
                    });
                    ui.add_space(2.0);
                    let tested = GenParams::prompt_writer();
                    let is_tested = params.thinking == tested.thinking
                        && params.temperature == tested.temperature
                        && params.top_p == tested.top_p
                        && params.top_k == tested.top_k
                        && params.min_p == tested.min_p
                        && params.max_tokens == tested.max_tokens
                        && params.seed == tested.seed;
                    let reset = ui
                        .add_enabled(!is_tested, egui::Button::new("Reset to tested defaults"))
                        .on_hover_text("Thinking off, temperature 0.4, top-p 0.95, top-k 20, min-p 0.05, 1500 tokens");
                    if reset.clicked() {
                        *params = GenParams { stream: params.stream, ..tested };
                        *dirty = true;
                    }
                },
            );
        });
    }

    fn check(&mut self, ui: &mut Ui) {
        let ws = &mut *self.ws;
        let running = self.view.running;
        widgets::panel(ui, "write-check", "Check", true, |ui| {
            let Some(report) = ws.lint.as_ref().filter(|_| !running) else {
                let text = if running {
                    "The prompt is checked when Bonsai has finished."
                } else {
                    "No prompt yet. Write one, or paste a prompt into the Prompt area to check it."
                };
                ui.label(RichText::new(text).color(Palette::TEXT_DIM));
                return;
            };
            let color = score_color(report);
            ui.horizontal(|ui| {
                ui.label(RichText::new(report.score.to_string()).size(30.0).strong().color(color));
                ui.vertical(|ui| {
                    let verdict = if report.errors() > 0 {
                        "Fix the errors before rendering"
                    } else if report.warnings() > 0 {
                        "Usable - check the warnings"
                    } else {
                        "Ready for ComfyUI"
                    };
                    ui.label(RichText::new(verdict).color(color));
                    ui.horizontal(|ui| {
                        let errors = report.errors();
                        let warnings = report.warnings();
                        let infos = report.issues.len() - errors - warnings;
                        let faint = Palette::TEXT_FAINT;
                        let error_color = if errors > 0 { Palette::ERR } else { faint };
                        widgets::badge(ui, &plural(errors, "error", "errors"), error_color);
                        let warning_color = if warnings > 0 { Palette::WARN } else { faint };
                        widgets::badge(ui, &plural(warnings, "warning", "warnings"), warning_color);
                        if infos > 0 {
                            widgets::badge(ui, &plural(infos, "hint", "hints"), Palette::INFO);
                        }
                    });
                });
            });
            let ctx = ws.prompt_context();
            ui.label(RichText::new(context_line(&ctx)).small().color(Palette::TEXT_DIM))
                .on_hover_text("What the prompt is checked against");
            ui.add_space(2.0);
            widgets::thin_separator(ui);
            ui.add_space(2.0);
            if report.issues.is_empty() {
                icon_line(ui, icons::CHECK, Palette::OK, "No problems found.");
            }
            let mut issues: Vec<&LintIssue> = report.issues.iter().collect();
            issues.sort_by_key(|i| (i.severity, i.line.unwrap_or(usize::MAX)));
            let mut jump = None;
            for (i, issue) in issues.into_iter().enumerate() {
                if issue_row(ui, i, issue) {
                    jump = issue.line;
                }
            }
            if jump.is_some() {
                ws.jump_to_line = jump;
            }
        });
    }

    fn fix(&mut self, ui: &mut Ui) {
        let ws = &mut *self.ws;
        let view = self.view;
        let acts = &mut *self.acts;
        // Fixes for the kind of prompt on screen (a fix may have turned it into the other kind).
        let fixes = self.library.fixes_for(ws.prompt_context().mode);
        widgets::panel(ui, "write-fix", "Fix", true, |ui| {
            let has_prompt = !ws.prompt.trim().is_empty();
            // Small buttons in an even two-column grid (labels cut to fit; the tooltip has it all).
            const COLUMNS: usize = 2;
            const GAP: f32 = 3.0;
            let chip_w = ((ui.available_width() - GAP * (COLUMNS - 1) as f32) / COLUMNS as f32).floor();
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
                for row in fixes.chunks(COLUMNS) {
                    ui.horizontal(|ui| {
                        for fix in row {
                            let label = RichText::new(short_fix_label(fix)).size(12.0);
                            let resp = ui.add_sized([chip_w, 20.0], egui::Button::new(label).truncate());
                            let resp = resp.on_hover_ui(|ui| {
                                ui.set_max_width(320.0);
                                ui.label(fix.as_str());
                                ui.label(
                                    RichText::new(
                                        "Click to put it into the fix field, fill in the <...> parts, then Send fix.",
                                    )
                                    .small()
                                    .color(Palette::TEXT_DIM),
                                );
                            });
                            if resp.clicked() {
                                ws.fix_text = fix.clone();
                                ws.focus_fix = true;
                            }
                        }
                    });
                }
            });
            ui.add_space(4.0);
            let field = ui.add(
                TextEdit::multiline(&mut ws.fix_text)
                    .id(egui::Id::new(FIX_FIELD))
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .hint_text("Your own fix, e.g. Make her laugh at the end."),
            );
            if ws.focus_fix {
                field.request_focus();
                ws.focus_fix = false;
            }
            let has_fix = !ws.fix_text.trim().is_empty();
            let can_send = has_prompt && has_fix && !view.running && view.can_run;
            let why = if !has_prompt {
                "Write a prompt first: a fix is a follow-up on the current prompt"
            } else if !has_fix {
                "Pick a fix above or type your own"
            } else if view.running {
                "Wait until Bonsai has finished"
            } else {
                view.hint.as_ref().map_or("Bonsai cannot take a job now", |h| h.text.as_str())
            };
            ui.horizontal(|ui| {
                let button = egui::Button::new(format!("{} Send fix", icons::PLAY)).min_size(egui::vec2(104.0, 22.0));
                let send = ui
                    .add_enabled(can_send, button)
                    .on_hover_text("Send the fix as a follow-up on the current prompt (Ctrl+Enter in the fix field)")
                    .on_disabled_hover_text(why);
                if send.clicked() {
                    acts.send_fix = true;
                }
                if ui.add_enabled(has_fix, egui::Button::new("Clear")).on_hover_text("Empty the fix field").clicked() {
                    ws.fix_text.clear();
                }
            });
            if !has_prompt {
                ui.label(
                    RichText::new("A fix is a follow-up on the current prompt: write one first.")
                        .small()
                        .color(Palette::TEXT_DIM),
                );
            }
        });
    }

    fn checklist(&mut self, ui: &mut Ui) {
        let ws = &mut *self.ws;
        let mode = ws.spec.mode;
        let items: Vec<(String, String)> = self
            .library
            .checklist_items()
            .into_iter()
            .filter(|(section, _)| section_mode(section).is_none_or(|m| m == mode))
            .collect();
        let done = items.iter().filter(|(s, i)| ws.ticks.contains(&tick_key(s, i))).count();
        let title = format!("Checklist  ·  {done}/{}", items.len());
        widgets::panel(ui, "write-checklist", &title, false, |ui| {
            if items.is_empty() {
                ui.label(RichText::new("The checklist in the Library is empty.").color(Palette::TEXT_DIM));
            }
            let mut section: Option<&str> = None;
            for (sec, item) in &items {
                if section != Some(sec.as_str()) {
                    if section.is_some() {
                        ui.add_space(4.0);
                    }
                    widgets::section_label(ui, &section_title(sec));
                    section = Some(sec.as_str());
                }
                let key = tick_key(sec, item);
                let mut ticked = ws.ticks.contains(&key);
                if ui.checkbox(&mut ticked, item.as_str()).changed() {
                    if ticked {
                        ws.ticks.insert(key);
                    } else {
                        ws.ticks.remove(&key);
                    }
                }
            }
            ui.add_space(4.0);
            let clear = ui
                .add_enabled(done > 0, egui::Button::new("Clear"))
                .on_hover_text("Untick everything (for the next prompt)");
            if clear.clicked() {
                ws.ticks.clear();
            }
        });
    }

    fn last_run(&mut self, ui: &mut Ui) {
        let ws = &*self.ws;
        let acts = &mut *self.acts;
        widgets::panel(ui, "write-last-run", "Last run", true, |ui| {
            let Some(run) = &ws.last_run else {
                ui.label(RichText::new("Nothing written yet.").color(Palette::TEXT_DIM));
                return;
            };
            match &run.source {
                RunSource::Opened(date) => {
                    ui.label(RichText::new(format!("Opened from History · {date}")).small().color(Palette::TEXT_DIM));
                }
                RunSource::Fixed => {
                    ui.label(RichText::new("Follow-up fix").small().color(Palette::TEXT_DIM));
                }
                RunSource::Written => {}
            }
            match &run.timings {
                Some(t) => {
                    widgets::prop_row(ui, "Prompt", |ui| {
                        ui.label(format!("{} new", thousands(u64::from(t.prompt_n))));
                        ui.label(
                            RichText::new(format!("· {} cached", thousands(u64::from(t.cache_n))))
                                .color(Palette::TEXT_DIM),
                        )
                        .on_hover_text("Tokens llama-server reused from its cache (same system prompt)");
                    });
                    widgets::prop_row(ui, "Generated", |ui| {
                        ui.label(format!("{} tokens", thousands(u64::from(t.predicted_n))));
                    });
                    widgets::prop_row(ui, "Speed", |ui| {
                        ui.label(format!("{:.1} tok/s", t.predicted_per_second));
                        ui.label(
                            RichText::new(format!("· prompt {:.0}/s", t.prompt_per_second)).color(Palette::TEXT_DIM),
                        )
                        .on_hover_text("Generation speed · prompt processing speed (tokens per second)");
                    });
                }
                None => {
                    ui.label(RichText::new("No token numbers for this run.").color(Palette::TEXT_DIM));
                }
            }
            if let Some(ms) = run.elapsed_ms {
                widgets::prop_row(ui, "Time", |ui| {
                    ui.label(format!("{:.1} s", ms as f64 / 1000.0));
                });
            }
            if !run.finish_reason.is_empty() {
                widgets::prop_row(ui, "Finish", |ui| {
                    if run.finish_reason == "length" {
                        ui.label(RichText::new("length").color(Palette::WARN))
                            .on_hover_text(format!("Bonsai hit the {} token limit: raise Max tokens", run.max_tokens));
                    } else {
                        ui.label(run.finish_reason.as_str());
                    }
                });
            }
            widgets::prop_row(ui, "History", |ui| match &run.history_id {
                Some(id) => {
                    // The button stays at the right edge; a long id is cut (full id in the tooltip).
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Open").on_hover_text("Show it in the History workspace").clicked() {
                            acts.open = Some(Workspace::History);
                        }
                        let label = RichText::new(id).monospace().size(11.0).color(Palette::TEXT_DIM);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(label).truncate()).on_hover_text(id.as_str());
                        });
                    });
                }
                None => {
                    ui.label(RichText::new("not saved").color(Palette::TEXT_DIM))
                        .on_hover_text("Saving history is off in Settings, or the answer was empty");
                }
            });
        });
    }
}

/// One lint issue: severity icon, message, `code · line N`. Returns true when clicked.
fn issue_row(ui: &mut Ui, index: usize, issue: &LintIssue) -> bool {
    let color = widgets::severity_color(issue.severity);
    let background = ui.painter().add(egui::Shape::Noop);
    let inner = ui.horizontal_top(|ui| {
        ui.label(RichText::new(severity_icon(issue.severity)).color(color));
        ui.vertical(|ui| {
            ui.label(RichText::new(&issue.message).color(Palette::TEXT));
            let meta = match issue.line {
                Some(line) => format!("{} · line {line}", issue.code),
                None => issue.code.clone(),
            };
            ui.label(RichText::new(meta).small().color(Palette::TEXT_FAINT));
        });
    });
    let rect = inner.response.rect.expand2(egui::vec2(3.0, 1.0));
    let Some(line) = issue.line else {
        ui.add_space(2.0);
        return false;
    };
    let resp = ui.interact(rect, ui.id().with(("write-issue", index)), Sense::click());
    if resp.hovered() {
        ui.painter().set(background, egui::Shape::rect_filled(rect, CornerRadius::same(3), ROW_HOVER));
    }
    let clicked = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!("Select line {line} in the prompt"))
        .clicked();
    ui.add_space(2.0);
    clicked
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

/// Icon + wrapped text in one colour.
fn icon_line(ui: &mut Ui, icon: &str, color: Color32, text: &str) {
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(icon).color(color));
        ui.vertical(|ui| ui.label(RichText::new(text).color(color)));
    });
}

fn severity_icon(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "✖",
        Severity::Warning => icons::WARN,
        Severity::Info => icons::INFO,
    }
}

fn score_color(report: &LintReport) -> Color32 {
    if report.errors() > 0 {
        Palette::ERR
    } else if report.score >= 90 {
        Palette::OK
    } else {
        Palette::WARN
    }
}

fn state_color(state: &ServerState) -> Color32 {
    match state {
        ServerState::Ready => Palette::OK,
        ServerState::Starting | ServerState::Loading | ServerState::Stopping => Palette::WARN,
        ServerState::Failed(_) => Palette::ERR,
        ServerState::Stopped => Palette::TEXT_FAINT,
    }
}

/// Short state label (the failure message is shown under it).
fn state_text(state: &ServerState) -> String {
    match state {
        ServerState::Failed(_) => "Failed".to_string(),
        other => other.label(),
    }
}

/// Loaded model (when Ready), else the model the launcher would load. Returns (text, tooltip).
fn model_text(status: &StatusSnapshot) -> (RichText, String) {
    if let Some(props) = &status.props {
        let tip = if props.model_ftype.is_empty() {
            props.model_path.clone()
        } else {
            format!("{}\n{}", props.model_path, props.model_ftype)
        };
        return (RichText::new(&props.model_name), tip);
    }
    match status.install.as_ref().and_then(|i| i.primary_model()) {
        Some(path) => {
            (RichText::new(file_name(path)).color(Palette::TEXT_DIM), format!("{}\n(not loaded)", path.display()))
        }
        None => (RichText::new("-").color(Palette::TEXT_DIM), "No model found".to_string()),
    }
}

/// `Ref N` (r2v) / `Picture N` (f2v).
fn ref_name(mode: Mode, index: usize) -> String {
    match mode {
        Mode::R2v => format!("Ref {}", index + 1),
        Mode::F2v => format!("Picture {}", index + 1),
    }
}

/// Placeholder of a reference description.
fn role_hint(mode: Mode, index: usize, role: RefRole) -> &'static str {
    if mode == Mode::F2v {
        return if index == 0 {
            "What the first frame shows exactly: who, clothes, pose, place, light, framing"
        } else {
            "What the last frame shows: pose, place, light, framing"
        };
    }
    match role {
        RefRole::Person => "The person: age, hair, face, figure, clothes",
        RefRole::SamePerson => "Which view of the same person: full body / side view, outfit",
        RefRole::Place => "The place: empty, its key features, light, time of day",
        RefRole::PersonAndPlace => "Who (look, clothes) and the place they are in",
        RefRole::Outfit => "The outfit or clothing item to put on someone",
        RefRole::Prop => "The object",
        RefRole::Style => "The look to copy: colours, light, grain",
        RefRole::FirstFrame => "What the first frame shows exactly: who, pose, where, framing",
        RefRole::LastFrame => "What the last frame shows",
        RefRole::Keyframe => "What this later shot shows",
        RefRole::Storyboard => "The shots and their framing",
    }
}

/// `6 s → 6.58 s`.
fn duration_label(seconds: u32) -> String {
    format!("{seconds} s  →  {} s", lengths::format_real(seconds as f32))
}

fn cuts_label(cuts: u32) -> String {
    match cuts {
        0 => "one take".to_string(),
        1 => "1 cut".to_string(),
        n => format!("{n} cuts"),
    }
}

/// Typed cuts: a number, or "one take".
fn parse_cuts(text: &str) -> Option<f64> {
    let text = text.trim().to_ascii_lowercase();
    if text.starts_with("one") || text.starts_with("no") {
        return Some(0.0);
    }
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<f64>().ok()
}

/// `r2v · 6 s (6.58 s) · 2 refs`.
fn context_line(ctx: &LintContext) -> String {
    let mut parts = vec![ctx.mode.as_str().to_string()];
    if let Some(d) = ctx.duration_s.filter(|d| d.is_finite() && *d > 0.0) {
        let asked = if d.fract() == 0.0 { format!("{d:.0}") } else { format!("{d:.1}") };
        parts.push(format!("{asked} s ({} s)", lengths::format_real(d)));
    }
    match ctx.mode {
        Mode::R2v => {
            if let Some(n) = ctx.ref_count {
                parts.push(plural(n, "ref", "refs"));
            }
            if ctx.first_frame_pinned == Some(true) {
                parts.push("first-frame pin".to_string());
            }
        }
        Mode::F2v => match ctx.has_last_frame {
            Some(true) => parts.push("first + last frame".to_string()),
            Some(false) => parts.push("first frame only".to_string()),
            None => {}
        },
    }
    parts.join(" · ")
}

/// Chip label of a fix: its first clause (the part before `: ` / ` (` / `. `). The button cuts
/// it to its width; the tooltip shows the whole fix.
fn short_fix_label(fix: &str) -> String {
    let mut head = fix.trim();
    for separator in [": ", " (", ". "] {
        if let Some(i) = head.find(separator) {
            if i >= 8 {
                head = &head[..i];
            }
        }
    }
    // `<...>` placeholders read better as one ellipsis character when the label is cut.
    head.trim_end_matches(['.', ':', ' ']).replace("<...>", "<…>")
}

/// Mode of a checklist section (`r2v prompt (...)` / `f2v prompt (...)`), `None` for shared ones.
fn section_mode(section: &str) -> Option<Mode> {
    let lower = section.trim().to_ascii_lowercase();
    if lower.starts_with("r2v") {
        Some(Mode::R2v)
    } else if lower.starts_with("f2v") {
        Some(Mode::F2v)
    } else {
        None
    }
}

/// Section name without the long `(workflow: ...)` part.
fn section_title(section: &str) -> String {
    let short = match section.find(" (") {
        Some(i) if i > 0 => &section[..i],
        _ => section,
    };
    if short.trim().is_empty() {
        "Checks".to_string()
    } else {
        short.trim().to_string()
    }
}

fn tick_key(section: &str, item: &str) -> String {
    format!("{section}\n{item}")
}

/// Char range of a 1-based line (for the editor cursor).
fn line_char_range(text: &str, line: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    for (i, row) in text.split('\n').enumerate() {
        let len = row.chars().count();
        if i + 1 == line {
            return Some((start, start + len));
        }
        start += len + 1;
    }
    None
}

/// The selection between char offsets `a` and `b` (either order) covers all of `text`.
fn selects_all(a: usize, b: usize, text: &str) -> bool {
    a.min(b) == 0 && a.max(b) >= text.chars().count()
}

/// Text rows that fill `height` in the given style (at least 3).
fn rows_for(ui: &Ui, height: f32, style: &TextStyle) -> usize {
    let row = ui.text_style_height(style).max(1.0);
    (((height - 8.0) / row).floor() as usize).max(3)
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// `1 ref` / `2 refs`.
fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `3354` -> `3,354`.
fn thousands(n: u64) -> String {
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

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_labels_are_short() {
        assert_eq!(
            short_fix_label("This is a single take: use only [Shot 1] and no times inside it."),
            "This is a single take"
        );
        assert_eq!(short_fix_label("Remove the music (non_diegetic_music: N/A)."), "Remove the music");
        assert_eq!(
            short_fix_label("Change her outfit to <...> and mark her partially_preserved."),
            "Change her outfit to <…> and mark her partially_preserved"
        );
        assert_eq!(short_fix_label("Give it a film look: 35mm, soft grain."), "Give it a film look");
    }

    #[test]
    fn line_ranges_count_chars() {
        let text = "a\nbé c\n\nd";
        assert_eq!(line_char_range(text, 1), Some((0, 1)));
        assert_eq!(line_char_range(text, 2), Some((2, 6)));
        assert_eq!(line_char_range(text, 4), Some((8, 9)));
        assert_eq!(line_char_range(text, 5), None);
    }

    #[test]
    fn mode_switch_keeps_r2v_roles() {
        let mut ws = WriteState { spec: default_spec(Mode::R2v, 6), ..WriteState::default() };
        ws.spec.refs.push(RefImage { role: RefRole::Place, ..RefImage::default() });
        ws.set_mode(Mode::F2v);
        assert_eq!(ws.spec.refs[0].role, RefRole::FirstFrame);
        assert_eq!(ws.spec.refs[1].role, RefRole::LastFrame);
        ws.set_mode(Mode::R2v);
        assert_eq!(ws.spec.refs[0].role, RefRole::Person);
        assert_eq!(ws.spec.refs[1].role, RefRole::Place);
        assert!(!ws.spec.first_frame_pinned);
    }

    #[test]
    fn f2v_entry_switched_to_r2v_pins_the_first_frame() {
        let mut ws = WriteState { spec: default_spec(Mode::F2v, 6), ..WriteState::default() };
        ws.spec.refs.push(RefImage { role: RefRole::LastFrame, ..RefImage::default() });
        ws.set_mode(Mode::R2v);
        assert_eq!(ws.spec.refs[0].role, RefRole::FirstFrame);
        assert_eq!(ws.spec.refs[1].role, RefRole::Keyframe);
        assert!(ws.spec.first_frame_pinned);
    }

    #[test]
    fn numbers_and_context() {
        assert_eq!(thousands(3354), "3,354");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1_234_567), "1,234,567");
        let ctx = LintContext { mode: Mode::R2v, duration_s: Some(6.0), ref_count: Some(2), ..LintContext::default() };
        assert_eq!(context_line(&ctx), "r2v · 6 s (6.58 s) · 2 refs");
        assert_eq!(parse_cuts("one take"), Some(0.0));
        assert_eq!(parse_cuts("2 cuts"), Some(2.0));
    }

    #[test]
    fn loaded_entry_keeps_form_text_and_fix_chain() {
        let spec = RequestSpec { action: "she waves".into(), ..RequestSpec::default() };
        let mut entry =
            HistoryEntry::new(Mode::R2v, "Ref 1: x.\n\nFollow-up fix: Make it longer.", "subject_definitions:\n");
        entry.spec = Some(spec.clone());
        let mut ws = WriteState::default();
        ws.load_entry(&entry);
        assert_eq!(ws.text, "Ref 1: x.");
        assert!(!ws.edited);
        assert_eq!(ws.tab, RequestTab::Builder);
        assert_eq!(ws.parent_id.as_deref(), Some(entry.id.as_str()));
        assert_eq!(ws.last_sent.as_ref().map(|s| s.text.as_str()), Some("Ref 1: x."));
        assert!(ws.lint.is_some());
        ws.sync_text();
        assert_eq!(ws.text, spec.build_text());

        let plain = HistoryEntry::new(Mode::F2v, "Picture 1: a man.", "");
        ws.load_entry(&plain);
        assert!(ws.edited);
        assert_eq!(ws.tab, RequestTab::Text);
        assert_eq!(ws.spec.mode, Mode::F2v);
        assert!(ws.lint.is_none());
    }

    #[test]
    fn demo_prompt_lints_clean_for_its_form() {
        let ctx = LintContext { mode: Mode::R2v, duration_s: Some(6.0), ref_count: Some(2), ..LintContext::default() };
        let report = lint::lint(DEMO_PROMPT, &ctx);
        assert_eq!(report.errors(), 0, "{:?}", report.issues);
    }

    /// A workspace with the default r2v form (6 s, 1 ref) and its text, as after `init`.
    fn fresh() -> WriteState {
        let mut ws = WriteState { spec: default_spec(Mode::R2v, 6), initialized: true, ..WriteState::default() };
        ws.sync_text();
        ws
    }

    fn outcome(prompt: &str, ctx: &LintContext) -> WriteOutcome {
        WriteOutcome {
            prompt: prompt.to_string(),
            reasoning: String::new(),
            lint: lint::lint(prompt, ctx),
            result: bh3::client::ChatResult { finish_reason: "stop".into(), ..Default::default() },
            history_id: None,
        }
    }

    fn running(job: WriteJob, sent: Option<SentRequest>, previous: PromptSnapshot) -> RunningJob {
        RunningJob { id: 1, job, sent, phase: String::new(), started: 0.0, cancel_requested: false, previous }
    }

    #[test]
    fn templates_and_typed_requests_are_checked_against_what_they_say() {
        let mut ws = fresh();
        let template = Template {
            id: "t".into(),
            title: "person + place photo".into(),
            mode: Mode::R2v,
            text: "Ref 1: a woman. Ref 2: a cafe (place).\n10 s, one take: she sits down.".into(),
        };
        ws.use_template(&template);
        let ctx = ws.request_lint_context();
        assert_eq!((ctx.duration_s, ctx.ref_count), (Some(10.0), Some(2)), "not the form's 6 s / 1 ref");

        // Typed from scratch: only what the text says (the form's 1 ref is not used).
        ws.rebuild_text();
        ws.text.clear();
        ws.edited = true;
        ws.text_from_form = false;
        ws.text = "she waves, 8 s".into();
        let ctx = ws.request_lint_context();
        assert_eq!((ctx.duration_s, ctx.ref_count), (Some(8.0), None));

        // An edit of the form's own text: the text's facts, the rest from the form.
        let mut ws = fresh();
        ws.spec.refs.push(RefImage { role: RefRole::Place, description: "a cafe".into(), ..RefImage::default() });
        ws.spec.first_frame_pinned = true;
        ws.spec.refs[0].role = RefRole::FirstFrame;
        ws.sync_text();
        ws.text = ws.text.replace("6 s", "8 s");
        ws.edited = true;
        let ctx = ws.request_lint_context();
        assert_eq!((ctx.duration_s, ctx.ref_count, ctx.first_frame_pinned), (Some(8.0), Some(2), Some(true)));
    }

    #[test]
    fn fixes_follow_the_prompt_on_screen() {
        let mut ws = fresh();
        let sent_ctx = ws.spec.lint_context();
        ws.last_sent = Some(SentRequest {
            mode: Mode::R2v,
            text: ws.text.clone(),
            spec: Some(ws.spec.clone()),
            form_text: true,
            lint_ctx: sent_ctx,
        });
        ws.prompt = "subject_definitions:\n".into();
        // An earlier fix made it a 10 s f2v prompt.
        ws.prompt_ctx = Some(LintContext { mode: Mode::F2v, duration_s: Some(10.0), ..LintContext::default() });
        let job = fix_job(&ws, "Make her laugh".into(), GenParams::prompt_writer());
        assert_eq!(job.mode, Mode::F2v);
        assert_eq!(job.lint_ctx.duration_s, Some(10.0));
        let followup = job.followup.as_ref().expect("a follow-up");
        assert_eq!(followup.previous_request, ws.text);
        assert!(job.spec.is_some(), "the form goes to History");

        // Cleared and pasted over: no earlier request, no pictures, no parent.
        ws.parent_id = Some("20261028-1".into());
        ws.clear_prompt();
        ws.prompt = "subject_definitions:\n".into();
        let job = fix_job(&ws, "Remove the music".into(), GenParams::prompt_writer());
        let followup = job.followup.as_ref().expect("a follow-up");
        assert!(followup.previous_request.is_empty());
        assert!(followup.parent_id.is_none());
        assert!(job.spec.is_none() && job.images.is_empty());
        assert_eq!(job.lint_ctx, ws.spec.lint_context(), "checked like the Check panel checks it");
        assert!(selects_all(9, 0, "123456789") && !selects_all(1, 9, "123456789"));
    }

    #[test]
    fn a_finished_fix_keeps_a_new_fix_and_an_edited_prompt_can_come_back() {
        let mut ws = fresh();
        let ctx = ws.spec.lint_context();
        let mut job = WriteJob::from_text(Mode::R2v, "Remove the music", GenParams::prompt_writer());
        job.followup = Some(FollowUp {
            previous_request: ws.text.clone(),
            previous_output: "old".into(),
            fix: "Remove the music".into(),
            parent_id: None,
        });
        ws.fix_text = "Make it 10 s with 2 cuts".into();
        let previous = PromptSnapshot { prompt: "my edited prompt".into(), edited: true, ..PromptSnapshot::default() };
        ws.finish(running(job.clone(), None, previous), &outcome("subject_definitions:\nnew", &ctx));
        assert_eq!(ws.fix_text, "Make it 10 s with 2 cuts", "typed while the fix ran");
        assert_eq!(ws.prompt, "subject_definitions:\nnew");
        assert!(ws.undo.as_ref().is_some_and(|u| !u.whole));
        ws.apply_undo();
        assert_eq!(ws.prompt, "my edited prompt");
        assert!(ws.prompt_edited);

        // The sent fix is cleared; an unedited prompt is replaced without an offer.
        ws.fix_text = "Remove the music".into();
        let previous = PromptSnapshot { prompt: "written".into(), ..PromptSnapshot::default() };
        ws.finish(running(job, None, previous), &outcome("subject_definitions:\nnewer", &ctx));
        assert!(ws.fix_text.is_empty());
        assert!(ws.undo.is_none());
    }

    #[test]
    fn open_in_write_can_be_undone() {
        let mut ws = fresh();
        let untouched = HistoryEntry::new(Mode::F2v, "Picture 1: a man.", "x");
        ws.load_entry(&untouched);
        assert!(ws.undo.is_none(), "nothing to keep");

        let mut ws = fresh();
        ws.spec.action = "she waves".into();
        ws.prompt = "my prompt".into();
        ws.prompt_edited = true;
        ws.load_entry(&untouched);
        assert_eq!(ws.spec.mode, Mode::F2v);
        assert!(ws.undo.as_ref().is_some_and(|u| u.whole && u.message.contains("from History")));
        ws.apply_undo();
        assert_eq!((ws.spec.mode, ws.spec.action.as_str(), ws.prompt.as_str()), (Mode::R2v, "she waves", "my prompt"));
        assert!(ws.last_sent.is_none() && ws.parent_id.is_none());
    }

    #[test]
    fn an_earlier_prompt_can_be_shown_and_undone() {
        let mut ws = fresh();
        ws.prompt = "on screen".into();
        let ctx = ws.spec.lint_context();
        let job = WriteJob::from_text(Mode::R2v, "a request", GenParams::prompt_writer());
        let sent = SentRequest { mode: Mode::R2v, text: "a request".into(), ..SentRequest::default() };
        let earlier =
            Earlier { job: running(job, Some(sent), PromptSnapshot::default()), outcome: outcome("earlier", &ctx) };
        ws.earlier = Some(Box::new(earlier));
        ws.show_earlier();
        assert_eq!(ws.prompt, "earlier");
        assert_eq!(ws.last_sent.as_ref().map(|s| s.text.as_str()), Some("a request"));
        ws.apply_undo();
        assert_eq!(ws.prompt, "on screen");
    }

    #[test]
    fn drafts_round_trip_and_bad_ones_are_kept_aside() {
        let mut ws = fresh();
        ws.spec.action = "she waves".into();
        ws.use_template(&Template { id: "t".into(), title: "t".into(), mode: Mode::R2v, text: "Ref 1: x.".into() });
        ws.prompt = "subject_definitions:\n".into();
        ws.prompt_ctx = Some(LintContext { mode: Mode::R2v, duration_s: Some(10.0), ..LintContext::default() });
        ws.last_sent = Some(SentRequest { mode: Mode::R2v, text: "Ref 1: x.".into(), ..SentRequest::default() });
        ws.fix_text = "Remove the music".into();

        let dir = std::env::temp_dir().join(format!("bh3-draft-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(DRAFT_FILE);
        let json = serde_json::to_string_pretty(&ws.snapshot()).expect("json");
        write_draft(&path, &json).expect("write");
        assert!(!dir.join("draft.json.tmp").exists());

        let mut back = fresh();
        back.restore(read_draft(&path).expect("a draft"), true);
        assert_eq!(serde_json::to_string_pretty(&back.snapshot()).expect("json"), json);
        assert_eq!((back.tab, back.edited, back.text_from_form), (RequestTab::Text, true, false));
        assert!(back.lint.is_some(), "the restored prompt is checked");

        std::fs::write(&path, "{ not json").expect("write");
        assert!(read_draft(&path).is_none());
        assert!(dir.join("draft.json.bad").is_file() && !path.exists());
        assert!(read_draft(&path).is_none(), "no draft");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
