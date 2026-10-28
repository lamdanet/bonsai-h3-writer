//! Chat workspace: free chat with Bonsai (a full replacement for its web UI).
//!
//! Layout (Blender style): conversation list on the left, messages and the input in the centre,
//! the N-panel (system prompt, generation settings, context usage) on the right.
//!
//! - Conversations are saved as `config_dir()\chats\<id>.json` (temp file + rename) after every
//!   finished message, rename, clear and settings change, and read the first time the workspace
//!   is shown. A fresh "New chat" is only written once it has content.
//! - One chat job at a time: the answer streams into an assistant placeholder (`JobDelta`) and is
//!   finished by `ChatDone` / `JobFailed` / `JobCancelled` (failures and stops are toasted here).
//! - Keys: Enter sends (Shift+Enter: new line, Ctrl+Enter also sends), Esc stops the answer,
//!   F2 renames the chat, Delete (pointer over the list) deletes it.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use bh3::client::Timings;
use bh3::gen::{ChatMessage, GenParams, ImageAttachment, Mode, Role, ThinkingMode};
use bh3::library::Library;
use bh3::service::{Event, JobId, JobKind};
use chrono::Datelike;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Id, Key, Label, Layout, Margin, Modifiers, Rect, Response,
    RichText, Sense, Stroke, Ui,
};
use serde::{Deserialize, Serialize};

use super::app::{AppState, ToastKind, Workspace};
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Title of a conversation that has not been named yet (replaced by the first message).
const DEFAULT_TITLE: &str = "New chat";
const INPUT_HINT: &str = "Message Bonsai... (Enter to send, Shift+Enter for a new line)";
const ATTACH_TOOLTIP: &str =
    "Add pictures for Bonsai's vision input (~1,100 tokens each).\nYou can also drop image files on the window.";
const INPUT_ID: &str = "bh3-chat-input";
const RENAME_ID: &str = "bh3-chat-rename";
/// Widest message column; wider areas centre it.
const COLUMN_MAX: f32 = 900.0;
/// Tallest the message box grows before it scrolls.
const INPUT_MAX_HEIGHT: f32 = 190.0;
/// Rough cost of one picture in Bonsai's context.
const TOKENS_PER_IMAGE: u64 = 1_100;
/// Height of a row in the conversation list.
const ROW_H: f32 = 38.0;
/// Files the image loader can show as thumbnails.
const PREVIEW_EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
/// Files Bonsai's vision input accepts (see `ImageAttachment::from_file`).
const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "gif", "bmp"];
/// Shown in the message box with `BH3_DEMO=1` (screenshots only).
const DEMO_INPUT: &str = "Good. Now make it 10 s: add a slow push-in while she turns the page, keep the rain sound.";

const ROW_SELECTED: Color32 = Color32::from_rgb(54, 80, 124);
const ROW_HOVER: Color32 = Color32::from_rgb(52, 52, 52);
const CARD_USER: Color32 = Color32::from_rgb(46, 50, 57);
const CARD_ASSISTANT: Color32 = Color32::from_rgb(50, 50, 50);
const CARD_SYSTEM: Color32 = Color32::from_rgb(39, 39, 39);
const CARD_ERROR: Color32 = Color32::from_rgb(62, 42, 40);
const THINK_FILL: Color32 = Color32::from_rgb(40, 40, 40);
const DANGER: Color32 = Color32::from_rgb(170, 60, 50);

// ---------------------------------------------------------------------------------------------
// Saved data
// ---------------------------------------------------------------------------------------------

/// Which system message a conversation sends first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SystemChoice {
    /// No system message.
    #[default]
    None,
    /// The Library's H3 r2v writer prompt.
    H3R2v,
    /// The Library's H3 f2v writer prompt.
    H3F2v,
    /// The user's own text.
    Custom(String),
}

impl SystemChoice {
    /// The system message text, `None` when nothing is sent.
    pub fn text<'a>(&'a self, library: &'a Library) -> Option<&'a str> {
        match self {
            SystemChoice::None => None,
            SystemChoice::H3R2v => Some(library.system_prompt(Mode::R2v)),
            SystemChoice::H3F2v => Some(library.system_prompt(Mode::F2v)),
            SystemChoice::Custom(text) => Some(text.as_str()).filter(|t| !t.trim().is_empty()),
        }
    }

    /// Short name for the header badge, `None` without a system message.
    pub fn badge(&self) -> Option<&'static str> {
        match self {
            SystemChoice::None => None,
            SystemChoice::H3R2v => Some("H3 r2v writer"),
            SystemChoice::H3F2v => Some("H3 f2v writer"),
            SystemChoice::Custom(_) => Some("Custom system"),
        }
    }

    fn kind(&self) -> SystemKind {
        match self {
            SystemChoice::None => SystemKind::None,
            SystemChoice::H3R2v => SystemKind::R2v,
            SystemChoice::H3F2v => SystemKind::F2v,
            SystemChoice::Custom(_) => SystemKind::Custom,
        }
    }
}

/// [`SystemChoice`] without the custom text (for the preset dropdown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemKind {
    None,
    R2v,
    F2v,
    Custom,
}

impl SystemKind {
    const ALL: [SystemKind; 4] = [SystemKind::None, SystemKind::R2v, SystemKind::F2v, SystemKind::Custom];

    fn label(self) -> &'static str {
        match self {
            SystemKind::None => "None",
            SystemKind::R2v => "H3 r2v writer",
            SystemKind::F2v => "H3 f2v writer",
            SystemKind::Custom => "Custom",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            SystemKind::None => "Plain chat: no system message",
            SystemKind::R2v => "The Library's system prompt for reference-image (r2v) prompts",
            SystemKind::F2v => "The Library's system prompt for first-frame (f2v) prompts",
            SystemKind::Custom => "Your own system message",
        }
    }

    fn choice(self, custom: &str) -> SystemChoice {
        match self {
            SystemKind::None => SystemChoice::None,
            SystemKind::R2v => SystemChoice::H3R2v,
            SystemKind::F2v => SystemChoice::H3F2v,
            SystemKind::Custom => SystemChoice::Custom(custom.to_string()),
        }
    }
}

/// One message of a saved conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatEntry {
    pub role: Role,
    pub content: String,
    /// Bonsai's thinking text (empty with thinking off).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    /// Pictures sent with a user message (read again for every request).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<Timings>,
    /// Why the answer failed (shown as a red card).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The user stopped the answer: the text is partial.
    #[serde(skip_serializing_if = "is_false")]
    pub stopped: bool,
    /// `stop`, or `length` when the answer hit Max tokens.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub finish_reason: String,
    /// Wall-clock time of the request in milliseconds (0 = unknown).
    #[serde(skip_serializing_if = "is_zero")]
    pub elapsed_ms: u64,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Default for ChatEntry {
    fn default() -> Self {
        Self {
            role: Role::User,
            content: String::new(),
            reasoning: String::new(),
            images: Vec::new(),
            timings: None,
            error: None,
            stopped: false,
            finish_reason: String::new(),
            elapsed_ms: 0,
        }
    }
}

impl ChatEntry {
    fn user(content: String, images: Vec<PathBuf>) -> Self {
        Self { role: Role::User, content, images, ..Self::default() }
    }

    /// The assistant entry an answer streams into.
    fn pending() -> Self {
        Self { role: Role::Assistant, ..Self::default() }
    }
}

/// A saved conversation (`chats\<id>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Conversation {
    /// File name without `.json`: a sortable local time like `20261028-142301-512`.
    pub id: String,
    pub title: String,
    /// RFC 3339 local time.
    pub created: String,
    /// RFC 3339 local time of the last message (the list is sorted by it).
    pub updated: String,
    pub system: SystemChoice,
    /// Generation settings of this conversation.
    pub params: GenParams,
    pub messages: Vec<ChatEntry>,
    /// The file exists (a fresh "New chat" is not written until it has content).
    #[serde(skip)]
    on_disk: bool,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: DEFAULT_TITLE.to_string(),
            created: String::new(),
            updated: String::new(),
            system: SystemChoice::None,
            params: GenParams::chat(),
            messages: Vec::new(),
            on_disk: false,
        }
    }
}

impl Conversation {
    fn new(id: String, params: GenParams) -> Self {
        let now = now_rfc3339();
        Self { id, created: now.clone(), updated: now, params, ..Self::default() }
    }

    /// A new, empty chat that was never saved.
    fn is_draft(&self) -> bool {
        !self.on_disk && self.messages.is_empty()
    }

    /// Milliseconds of the last change (for sorting).
    fn sort_key(&self) -> i64 {
        [self.updated.as_str(), self.created.as_str()]
            .into_iter()
            .find_map(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp_millis())
            .unwrap_or(0)
    }

    /// Timings of the newest answer (what the context bar shows).
    fn last_timings(&self) -> Option<&Timings> {
        self.messages.iter().rev().find_map(|m| m.timings.as_ref())
    }

    /// The whole conversation as plain text ("Copy all").
    fn transcript(&self) -> String {
        let mut out = format!("{}\n", self.title);
        if let Some(badge) = self.system.badge() {
            out.push_str(&format!("System prompt: {badge}\n"));
        }
        for entry in &self.messages {
            out.push('\n');
            out.push_str(role_name(entry.role));
            out.push_str(":\n");
            if !entry.content.trim().is_empty() {
                out.push_str(entry.content.trim());
                out.push('\n');
            }
            if !entry.images.is_empty() {
                let names: Vec<String> = entry.images.iter().map(|p| file_name(p)).collect();
                out.push_str(&format!("[pictures: {}]\n", names.join(", ")));
            }
            if let Some(error) = &entry.error {
                out.push_str(&format!("[error: {error}]\n"));
            }
            if entry.stopped {
                out.push_str("(stopped)\n");
            }
        }
        out
    }
}

/// `config_dir()\chats` (created on demand).
pub fn chats_dir() -> PathBuf {
    let dir = bh3::config::config_dir().join("chats");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn conversation_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Every readable conversation in `dir` (most recent first) and the number of unreadable files.
fn load_conversations(dir: &Path) -> (Vec<Conversation>, usize) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return (Vec::new(), 0);
    };
    let mut convs = Vec::new();
    let mut failed = 0;
    for entry in read.flatten() {
        let path = entry.path();
        let is_json = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("json"));
        if !is_json || !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        match read_conversation(&path) {
            Ok(mut conv) => {
                conv.id = stem;
                conv.on_disk = true;
                if conv.title.trim().is_empty() {
                    conv.title = DEFAULT_TITLE.to_string();
                }
                convs.push(conv);
            }
            Err(_) => failed += 1,
        }
    }
    sort_conversations(&mut convs);
    (convs, failed)
}

fn read_conversation(path: &Path) -> anyhow::Result<Conversation> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(bytes).with_context(|| format!("parsing {}", path.display()))
}

/// What is saved of `conv` while the answer at `streaming` still arrives: the conversation without
/// it, so a crash before the answer ends never leaves half an answer that looks complete.
fn without_streaming(conv: &Conversation, streaming: Option<usize>) -> std::borrow::Cow<'_, Conversation> {
    match streaming.filter(|&index| index < conv.messages.len()) {
        Some(index) => {
            let mut copy = conv.clone();
            copy.messages.remove(index);
            std::borrow::Cow::Owned(copy)
        }
        None => std::borrow::Cow::Borrowed(conv),
    }
}

fn save_conversation(dir: &Path, conv: &Conversation) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let json = serde_json::to_string_pretty(conv).context("serialising the chat")?;
    write_atomic(&conversation_path(dir, &conv.id), json.as_bytes())
}

/// Write through a temp file + rename so a crash never leaves half a file. Falls back to a plain
/// write when the rename fails (Windows refuses to replace a file another program has open).
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Remove `<id>.json` (a file that is already gone is fine).
fn delete_conversation_file(dir: &Path, id: &str) -> anyhow::Result<()> {
    let path = conversation_path(dir, id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("deleting {}", path.display())),
    }
}

fn sort_conversations(convs: &mut [Conversation]) {
    convs.sort_by(|a, b| b.sort_key().cmp(&a.sort_key()).then_with(|| b.id.cmp(&a.id)));
}

/// Fresh id from the local time, unique among `existing` and the files in `dir`.
fn new_conversation_id(existing: &[Conversation], dir: &Path) -> String {
    let base = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let taken = |id: &str| existing.iter().any(|c| c.id == id) || conversation_path(dir, id).exists();
    if !taken(&base) {
        return base;
    }
    (2u32..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap_or(base)
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

// ---------------------------------------------------------------------------------------------
// Workspace state
// ---------------------------------------------------------------------------------------------

/// The chat job in flight.
struct Running {
    job: JobId,
    conv_id: String,
    /// Index of the assistant entry that receives the answer.
    index: usize,
    phase: String,
    /// `AppState::time` when it was sent.
    started: f64,
    /// The thinking section was folded when the answer text began (only once, so the user can
    /// open it again while the answer streams).
    folded: bool,
}

/// Inline rename in the conversation list.
struct Rename {
    id: String,
    text: String,
    /// Focus the field (and select its text) on the next frame.
    focus: bool,
}

/// A question waiting in the confirmation popup.
#[derive(Clone)]
enum Confirm {
    Delete(String),
    Clear(String),
}

#[derive(Default)]
pub struct ChatState {
    loaded: bool,
    /// Most recently used first.
    convs: Vec<Conversation>,
    selected: Option<String>,
    input: String,
    attachments: Vec<PathBuf>,
    running: Option<Running>,
    rename: Option<Rename>,
    confirm: Option<Confirm>,
    /// Text of the Custom system message, kept while another preset is chosen.
    custom_draft: String,
    /// Conversation whose settings changed and still need saving, and when they last changed.
    dirty: Option<(String, f64)>,
    scroll_to_bottom: bool,
    focus_input: bool,
    /// Screen area of the conversation list last frame (Delete key applies over it).
    list_rect: Option<Rect>,
    /// egui pass in which the workspace was last shown (see [`came_back`]).
    shown_pass: Option<u64>,
    /// An IME composition (Japanese, Chinese...) is open in the message box.
    ime_composing: bool,
    /// This frame's Enter belongs to the IME: it commits the composition and must not send.
    ime_enter: bool,
}

impl ChatState {
    fn index_of(&self, id: &str) -> Option<usize> {
        self.convs.iter().position(|c| c.id == id)
    }

    fn conv(&self, id: &str) -> Option<&Conversation> {
        self.convs.iter().find(|c| c.id == id)
    }

    fn conv_mut(&mut self, id: &str) -> Option<&mut Conversation> {
        self.convs.iter_mut().find(|c| c.id == id)
    }

    fn selected_conv(&self) -> Option<&Conversation> {
        self.selected.as_deref().and_then(|id| self.conv(id))
    }

    fn is_running_in(&self, id: &str) -> bool {
        self.running.as_ref().is_some_and(|r| r.conv_id == id)
    }

    /// The entry the running answer streams into.
    fn running_entry(&mut self) -> Option<&mut ChatEntry> {
        let run = self.running.as_ref()?;
        let conv = self.convs.iter_mut().find(|c| c.id == run.conv_id)?;
        conv.messages.get_mut(run.index)
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    if came_back(ctx, &mut app.chat.shown_pass) {
        // Another workspace was shown (Ctrl+1..6 also work under the popup): the question must not
        // come back and take the next Enter, and a rename left open counts as a click elsewhere.
        app.chat.confirm = None;
        commit_rename(app);
    }
    ensure_loaded(app);
    track_ime(app, ctx);
    take_dropped_files(app, ctx);
    shortcuts(app, ctx);
    save_settings_when_idle(app, ctx);
    refresh_activity(app);

    let list = egui::SidePanel::left("chat-conversations")
        .resizable(true)
        .default_width(230.0)
        .width_range(180.0..=380.0)
        .frame(side_frame())
        .show(ctx, |ui| conversation_list(app, ui));
    app.chat.list_rect = Some(list.response.rect);
    egui::SidePanel::right("chat-npanel")
        .resizable(true)
        .default_width(300.0)
        .width_range(250.0..=460.0)
        .frame(side_frame())
        .show(ctx, |ui| n_panel(app, ui));
    egui::CentralPanel::default().frame(egui::Frame::new().fill(Palette::REGION)).show(ctx, |ui| main_area(app, ui));
    confirm_dialog(app, ctx);
}

pub fn on_event(app: &mut AppState, ev: &Event) {
    let Some(job) = app.chat.running.as_ref().map(|r| r.job) else {
        return;
    };
    match ev {
        Event::JobPhase { id, phase } if *id == job => {
            if let Some(run) = app.chat.running.as_mut() {
                run.phase.clone_from(phase);
            }
            refresh_activity(app);
        }
        Event::JobDelta { id, reasoning, content } if *id == job => {
            if let Some(entry) = app.chat.running_entry() {
                entry.reasoning.push_str(reasoning);
                entry.content.push_str(content);
            }
            refresh_activity(app);
        }
        Event::ChatDone { id, result } if *id == job => {
            if let Some(entry) = app.chat.running_entry() {
                entry.content.clone_from(&result.content);
                entry.reasoning.clone_from(&result.reasoning);
                entry.timings.clone_from(&result.timings);
                entry.finish_reason.clone_from(&result.finish_reason);
                entry.elapsed_ms = result.elapsed_ms;
            }
            app.last_timings.clone_from(&result.timings);
            if result.finish_reason == "length" {
                app.toast(ToastKind::Warn, "The answer hit the token limit: raise Max tokens in Generation.");
            }
            finish_running(app);
        }
        Event::JobFailed { id, error } if *id == job => {
            if let Some(entry) = app.chat.running_entry() {
                entry.error = Some(error.clone());
            }
            app.toast(ToastKind::Error, format!("Chat failed: {error}"));
            finish_running(app);
        }
        Event::JobCancelled(id) if *id == job => {
            if let Some(entry) = app.chat.running_entry() {
                entry.stopped = true;
            }
            app.toast(ToastKind::Info, "Answer stopped.");
            finish_running(app);
        }
        _ => {}
    }
}

/// Save what is still waiting: settings changed less than a moment ago and an open rename. `show`
/// only runs while the Chat workspace is visible, so the app calls this on exit and while another
/// workspace is shown (it does nothing when nothing is pending).
pub fn flush_pending(app: &mut AppState) {
    commit_rename(app);
    flush_dirty(app);
}

// ---------------------------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------------------------

fn demo_mode() -> bool {
    std::env::var("BH3_DEMO").is_ok_and(|v| v.trim() == "1")
}

/// Read the saved conversations the first time the workspace is shown.
fn ensure_loaded(app: &mut AppState) {
    if app.chat.loaded {
        return;
    }
    app.chat.loaded = true;
    let dir = chats_dir();
    let (convs, failed) = load_conversations(&dir);
    app.chat.convs = convs;
    if failed > 0 {
        app.toast(ToastKind::Warn, format!("{failed} saved chat(s) in {} could not be read.", dir.display()));
    }
    match app.chat.convs.first().map(|c| c.id.clone()) {
        Some(id) => app.chat.selected = Some(id),
        None => new_chat(app),
    }
    if demo_mode() && app.chat.input.is_empty() {
        app.chat.input = DEMO_INPUT.to_string();
    }
    app.chat.scroll_to_bottom = true;
}

/// Select (or create) an empty chat and focus the message box.
fn new_chat(app: &mut AppState) {
    flush_dirty(app);
    commit_rename(app);
    if let Some(id) = app.chat.convs.iter().find(|c| c.is_draft()).map(|c| c.id.clone()) {
        select(app, id);
    } else {
        let id = new_conversation_id(&app.chat.convs, &chats_dir());
        app.chat.convs.insert(0, Conversation::new(id.clone(), app.config.chat_params.clone()));
        select(app, id);
    }
    app.chat.focus_input = true;
}

fn select(app: &mut AppState, id: String) {
    if app.chat.selected.as_deref() == Some(id.as_str()) {
        return;
    }
    flush_dirty(app);
    commit_rename(app);
    // Unsaved empty chats disappear once another one is chosen.
    app.chat.convs.retain(|c| !c.is_draft() || c.id == id);
    app.chat.selected = Some(id);
    app.chat.scroll_to_bottom = true;
}

/// Save a conversation; `touch` marks it as just used (it moves to the top of the list).
fn save_by_id(app: &mut AppState, id: &str, touch: bool) {
    let dir = chats_dir();
    let streaming = app.chat.running.as_ref().filter(|r| r.conv_id == id).map(|r| r.index);
    let Some(conv) = app.chat.conv_mut(id) else {
        return;
    };
    if touch {
        conv.updated = now_rfc3339();
    }
    let result = save_conversation(&dir, &without_streaming(conv, streaming));
    if result.is_ok() {
        conv.on_disk = true;
    }
    if touch {
        sort_conversations(&mut app.chat.convs);
    }
    if let Err(e) = result {
        app.toast(ToastKind::Error, format!("Could not save the chat: {e:#}"));
    }
}

fn mark_dirty(app: &mut AppState, id: &str) {
    if app.chat.dirty.as_ref().is_some_and(|(d, _)| d != id) {
        flush_dirty(app);
    }
    app.chat.dirty = Some((id.to_string(), app.time));
}

/// Save pending settings changes now (only chats that are saved or have messages).
fn flush_dirty(app: &mut AppState) {
    let Some((id, _)) = app.chat.dirty.take() else {
        return;
    };
    if app.chat.conv(&id).is_some_and(|c| c.on_disk || !c.messages.is_empty()) {
        save_by_id(app, &id, false);
    }
}

/// Settings are saved a moment after the last change, not on every frame of a slider drag.
fn save_settings_when_idle(app: &mut AppState, ctx: &egui::Context) {
    let Some(since) = app.chat.dirty.as_ref().map(|d| d.1) else {
        return;
    };
    if app.time - since > 0.8 && !ctx.input(|i| i.pointer.any_down()) {
        flush_dirty(app);
    }
}

/// Keeps the status-bar text of a running answer current (called every frame, on any workspace).
pub fn refresh_activity(app: &mut AppState) {
    if let Some(run) = &app.chat.running {
        let secs = (app.time - run.started).max(0.0) as u64;
        app.activity = Some(format!("Chat: {} {}", run.phase, widgets::fmt_secs(secs)));
    }
}

/// The running answer ended (done, failed or stopped): save it and clear the status.
fn finish_running(app: &mut AppState) {
    let Some(run) = app.chat.running.take() else {
        return;
    };
    if app.activity.as_deref().is_some_and(|a| a.starts_with("Chat:")) {
        app.activity = None;
    }
    save_by_id(app, &run.conv_id, true);
}

fn stop_running(app: &mut AppState) {
    let Some(run) = app.chat.running.as_mut() else {
        return;
    };
    run.phase = bh3::service::phase::CANCELLING.to_string();
    let job = run.job;
    app.cancel(job);
    refresh_activity(app);
}

/// Send the message box (and attached pictures) in the selected chat.
fn send_message(app: &mut AppState) {
    if app.chat.running.is_some() {
        return;
    }
    let text = app.chat.input.trim().to_string();
    if text.is_empty() && app.chat.attachments.is_empty() {
        return;
    }
    if let Some(missing) = app.chat.attachments.iter().find(|p| !p.is_file()) {
        let message = format!("Picture not found: {}", missing.display());
        app.toast(ToastKind::Error, message);
        return;
    }
    flush_dirty(app);
    commit_rename(app);
    if app.chat.selected_conv().is_none() {
        new_chat(app);
    }
    let Some(id) = app.chat.selected.clone() else {
        return;
    };
    let images = std::mem::take(&mut app.chat.attachments);
    let Some(conv) = app.chat.conv_mut(&id) else {
        app.chat.attachments = images;
        return;
    };
    if conv.title.trim().is_empty() || conv.title == DEFAULT_TITLE {
        let title = bh3::history::title_from(&text);
        conv.title = if title.is_empty() { "Pictures".to_string() } else { title };
    }
    conv.messages.push(ChatEntry::user(text, images));
    app.chat.input.clear();
    start_answer(app, &id);
}

/// Send the conversation (which ends with a user message) and add the answer placeholder.
fn start_answer(app: &mut AppState, id: &str) {
    let Some(conv) = app.chat.conv(id) else {
        return;
    };
    let (messages, problems) = build_request(conv, &app.library);
    let params = conv.params.clone();
    if let Some(first) = problems.first() {
        let text = if problems.len() == 1 {
            format!("A picture was left out: {first}")
        } else {
            format!("{} pictures were left out: {first}", problems.len())
        };
        app.toast(ToastKind::Warn, text);
    }
    // Saved before the placeholder exists: a crash keeps the question, not an empty answer.
    save_by_id(app, id, true);
    let job = app.submit(JobKind::Chat { messages, params });
    let Some(conv) = app.chat.conv_mut(id) else {
        app.cancel(job);
        return;
    };
    conv.messages.push(ChatEntry::pending());
    let index = conv.messages.len() - 1;
    app.chat.running = Some(Running {
        job,
        conv_id: id.to_string(),
        index,
        phase: "Sending...".to_string(),
        started: app.time,
        folded: false,
    });
    app.chat.scroll_to_bottom = true;
    refresh_activity(app);
}

/// Messages for Bonsai: the system message, then the conversation. Failed or empty answers are
/// left out and neighbours with the same role are merged, so user and assistant alternate.
/// Returns the messages and the pictures that could not be read.
fn build_request(conv: &Conversation, library: &Library) -> (Vec<ChatMessage>, Vec<String>) {
    let mut out: Vec<ChatMessage> = Vec::new();
    let mut problems = Vec::new();
    if let Some(system) = conv.system.text(library) {
        out.push(ChatMessage::system(system));
    }
    for entry in &conv.messages {
        let message = match entry.role {
            Role::User => {
                let mut images = Vec::new();
                for path in &entry.images {
                    match ImageAttachment::from_file(path) {
                        Ok(image) => images.push(image),
                        Err(e) => problems.push(format!("{e:#}")),
                    }
                }
                ChatMessage::user(entry.content.trim()).with_images(images)
            }
            Role::Assistant => {
                if entry.error.is_some() || entry.content.trim().is_empty() {
                    continue;
                }
                // Sent as received, so llama-server can reuse its cache of the answer.
                ChatMessage::assistant(entry.content.as_str())
            }
            Role::System => ChatMessage::system(entry.content.trim()),
        };
        match out.last_mut() {
            Some(prev) if prev.role == message.role && message.role != Role::System => {
                if !message.content.is_empty() {
                    if !prev.content.is_empty() {
                        prev.content.push_str("\n\n");
                    }
                    prev.content.push_str(&message.content);
                }
                prev.images.extend(message.images);
            }
            _ => out.push(message),
        }
    }
    (out, problems)
}

fn can_regenerate(conv: &Conversation) -> bool {
    let skip = usize::from(conv.messages.last().is_some_and(|m| m.role == Role::Assistant));
    conv.messages.iter().rev().nth(skip).is_some_and(|m| m.role == Role::User)
}

/// Drop the last answer and ask again.
fn regenerate(app: &mut AppState, id: &str) {
    if app.chat.running.is_some() {
        return;
    }
    let Some(conv) = app.chat.conv_mut(id) else {
        return;
    };
    if !can_regenerate(conv) {
        return;
    }
    if conv.messages.last().is_some_and(|m| m.role == Role::Assistant) {
        conv.messages.pop();
    }
    start_answer(app, id);
}

/// Take every picture out of the chat and ask again: a picture Bonsai rejects (no vision projector,
/// more tokens than the context holds) is otherwise sent again with every later message.
fn retry_without_pictures(app: &mut AppState, id: &str) {
    if app.chat.running.is_some() {
        return;
    }
    let Some(conv) = app.chat.conv_mut(id) else {
        return;
    };
    if can_regenerate(conv) && drop_pictures(conv) > 0 {
        regenerate(app, id);
    }
}

/// Remove the pictures of every message (the files stay); returns how many were removed.
fn drop_pictures(conv: &mut Conversation) -> usize {
    conv.messages.iter_mut().map(|m| std::mem::take(&mut m.images).len()).sum()
}

fn start_rename(app: &mut AppState, id: &str) {
    let Some(title) = app.chat.conv(id).map(|c| c.title.clone()) else {
        return;
    };
    select(app, id.to_string());
    app.chat.rename = Some(Rename { id: id.to_string(), text: title, focus: true });
}

fn commit_rename(app: &mut AppState) {
    let Some(rename) = app.chat.rename.take() else {
        return;
    };
    let title = rename.text.trim();
    let Some(conv) = app.chat.conv_mut(&rename.id) else {
        return;
    };
    if title.is_empty() || conv.title == title {
        return;
    }
    conv.title = title.to_string();
    save_by_id(app, &rename.id, false);
}

fn duplicate(app: &mut AppState, id: &str) {
    if app.chat.is_running_in(id) {
        return;
    }
    let Some(mut copy) = app.chat.conv(id).cloned() else {
        return;
    };
    copy.id = new_conversation_id(&app.chat.convs, &chats_dir());
    copy.title = format!("{} (copy)", copy.title);
    let now = now_rfc3339();
    copy.created.clone_from(&now);
    copy.updated = now;
    copy.on_disk = false;
    let new_id = copy.id.clone();
    app.chat.convs.insert(0, copy);
    save_by_id(app, &new_id, false);
    select(app, new_id);
}

fn delete_conversation(app: &mut AppState, id: &str) {
    if app.chat.is_running_in(id) {
        if let Some(run) = app.chat.running.take() {
            app.cancel(run.job);
        }
        if app.activity.as_deref().is_some_and(|a| a.starts_with("Chat:")) {
            app.activity = None;
        }
    }
    if let Err(e) = delete_conversation_file(&chats_dir(), id) {
        app.toast(ToastKind::Error, format!("Could not delete the chat: {e:#}"));
        return;
    }
    let Some(pos) = app.chat.index_of(id) else {
        return;
    };
    app.chat.convs.remove(pos);
    if app.chat.dirty.as_ref().is_some_and(|(d, _)| d == id) {
        app.chat.dirty = None;
    }
    if app.chat.rename.as_ref().is_some_and(|r| r.id == id) {
        app.chat.rename = None;
    }
    if app.chat.selected.as_deref() == Some(id) {
        app.chat.selected = None;
        match app.chat.convs.get(pos).or_else(|| app.chat.convs.last()).map(|c| c.id.clone()) {
            Some(next) => select(app, next),
            None => new_chat(app),
        }
    }
    app.toast(ToastKind::Info, "Chat deleted.");
}

fn clear_conversation(app: &mut AppState, id: &str) {
    if app.chat.is_running_in(id) {
        return;
    }
    let Some(conv) = app.chat.conv_mut(id) else {
        return;
    };
    conv.messages.clear();
    save_by_id(app, id, false);
}

fn add_attachment(app: &mut AppState, path: PathBuf) {
    if !app.chat.attachments.contains(&path) {
        app.chat.attachments.push(path);
    }
}

/// Image files dropped on the window are attached.
fn take_dropped_files(app: &mut AppState, ctx: &egui::Context) {
    let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
    if dropped.is_empty() {
        return;
    }
    let mut skipped = 0;
    for path in dropped {
        if has_ext(&path, &IMAGE_EXTS) {
            add_attachment(app, path);
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        app.toast(ToastKind::Warn, "Only pictures can be attached (png, jpg, webp, gif, bmp).");
    }
}

/// Follow the IME composition of the message box (egui reports it as `Event::Ime`).
fn track_ime(app: &mut AppState, ctx: &egui::Context) {
    let (composing, seen) = ctx.input(|i| ime_step(app.chat.ime_composing, &i.events));
    app.chat.ime_enter = app.chat.ime_composing || seen;
    // A composition left open in a field that lost focus must not block Enter later.
    app.chat.ime_composing = composing && ctx.memory(|m| m.has_focus(Id::new(INPUT_ID)));
}

/// `composing` after this frame's events, and whether any of them came from the IME. A composition
/// is open from its first non-empty preedit text to its commit (Windows: Enter commits it).
fn ime_step(mut composing: bool, events: &[egui::Event]) -> (bool, bool) {
    let mut seen = false;
    for event in events {
        if let egui::Event::Ime(ime) = event {
            seen = true;
            match ime {
                egui::ImeEvent::Preedit(text) => composing = !text.is_empty(),
                egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => composing = false,
                // macOS sends no `Disabled` after a commit: only the preedit text opens one.
                egui::ImeEvent::Enabled => {}
            }
        }
    }
    (composing, seen)
}

fn shortcuts(app: &mut AppState, ctx: &egui::Context) {
    if app.chat.confirm.is_some() || app.show_setup {
        return;
    }
    if app.chat.running.is_some()
        && app.chat.rename.is_none()
        && app.esc_free
        && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
    {
        stop_running(app);
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) && !app.chat.ime_enter {
        send_message(app);
    }
    if ctx.wants_keyboard_input() || app.chat.rename.is_some() {
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F2)) {
        if let Some(id) = app.chat.selected.clone() {
            start_rename(app, &id);
        }
    }
    let over_list = match (app.chat.list_rect, ctx.pointer_hover_pos()) {
        (Some(rect), Some(pos)) => rect.contains(pos),
        _ => false,
    };
    if over_list && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete)) {
        if let Some(id) = app.chat.selected.clone() {
            app.chat.confirm = Some(Confirm::Delete(id));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Left: conversation list
// ---------------------------------------------------------------------------------------------

struct RowInfo {
    id: String,
    title: String,
    detail: String,
    running: bool,
}

enum ListAction {
    Select(String),
    StartRename(String),
    CommitRename,
    CancelRename,
    Duplicate(String),
    AskDelete(String),
}

fn conversation_list(app: &mut AppState, ui: &mut Ui) {
    let new_button =
        egui::Button::new(format!("{}  New chat", icons::ADD)).min_size(egui::vec2(ui.available_width(), 26.0));
    if ui.add(new_button).on_hover_text("Start a new conversation").clicked() {
        new_chat(app);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Conversations");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(app.chat.convs.len().to_string()).small().color(Palette::TEXT_FAINT));
        });
    });
    ui.add_space(2.0);

    let rows: Vec<RowInfo> = app
        .chat
        .convs
        .iter()
        .map(|c| {
            let n = c.messages.len();
            let when = fmt_when(if c.updated.is_empty() { &c.created } else { &c.updated });
            let count = if n == 0 { "empty".to_string() } else { format!("{n} message{}", plural(n)) };
            RowInfo {
                id: c.id.clone(),
                title: c.title.clone(),
                detail: if when.is_empty() { count } else { format!("{when}  ·  {count}") },
                running: app.chat.is_running_in(&c.id),
            }
        })
        .collect();
    let selected = app.chat.selected.clone();
    let mut actions = Vec::new();
    egui::ScrollArea::vertical().id_salt("chat-list-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for row in &rows {
            let is_selected = selected.as_deref() == Some(row.id.as_str());
            let rename = app.chat.rename.as_mut().filter(|r| r.id == row.id);
            actions.extend(conversation_row(ui, row, is_selected, rename));
        }
    });
    for action in actions {
        match action {
            ListAction::Select(id) => select(app, id),
            ListAction::StartRename(id) => start_rename(app, &id),
            ListAction::CommitRename => commit_rename(app),
            ListAction::CancelRename => app.chat.rename = None,
            ListAction::Duplicate(id) => duplicate(app, &id),
            ListAction::AskDelete(id) => app.chat.confirm = Some(Confirm::Delete(id)),
        }
    }
}

fn conversation_row(ui: &mut Ui, row: &RowInfo, selected: bool, rename: Option<&mut Rename>) -> Option<ListAction> {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), ROW_H), Sense::click());
    let fill = if selected {
        ROW_SELECTED
    } else if resp.hovered() {
        ROW_HOVER
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
    let title_color = if selected { Color32::WHITE } else { Palette::TEXT };
    let detail_color = if selected { Color32::from_gray(200) } else { Palette::TEXT_DIM };
    let left = rect.left() + 8.0;
    let right_pad = if row.running { 26.0 } else { 8.0 };
    let text_width = (rect.right() - right_pad - left).max(8.0);

    let mut action = None;
    let mut elided = false;
    match rename {
        Some(rename) => {
            let id = Id::new(RENAME_ID);
            let edit_rect =
                Rect::from_min_max(rect.min + egui::vec2(3.0, 2.0), egui::pos2(rect.right() - 3.0, rect.top() + 21.0));
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("chat-rename-row", row.id.as_str()))
                    .max_rect(edit_rect)
                    .layout(Layout::centered_and_justified(egui::Direction::TopDown)),
            );
            if rename.focus {
                // Start with the whole title selected, like Blender's F2.
                let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
                let end = egui::text::CCursor::new(rename.text.chars().count());
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
                state.store(ui.ctx(), id);
            }
            let edit = child.add(egui::TextEdit::singleline(&mut rename.text).id(id).margin(Margin::symmetric(4, 1)));
            if rename.focus {
                edit.request_focus();
                rename.focus = false;
            }
            if edit.lost_focus() {
                let cancelled = ui.input(|i| i.key_pressed(Key::Escape));
                action = Some(if cancelled { ListAction::CancelRename } else { ListAction::CommitRename });
            }
        }
        None => {
            let galley = single_line(ui, &row.title, FontId::proportional(13.5), title_color, text_width);
            elided = galley.elided;
            ui.painter().galley(egui::pos2(left, rect.top() + 3.0), galley, title_color);
        }
    }
    let detail = single_line(ui, &row.detail, FontId::proportional(11.0), detail_color, text_width);
    ui.painter().galley(egui::pos2(left, rect.top() + 22.0), detail, detail_color);
    if row.running {
        let spin = Rect::from_center_size(egui::pos2(rect.right() - 13.0, rect.center().y), egui::vec2(14.0, 14.0));
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt(("chat-row-spin", row.id.as_str())).max_rect(spin));
        child.add(egui::Spinner::new().size(12.0));
    }

    let resp = if elided { resp.on_hover_text(&row.title) } else { resp };
    if resp.double_clicked() {
        action = Some(ListAction::StartRename(row.id.clone()));
    } else if resp.clicked() {
        action = Some(ListAction::Select(row.id.clone()));
    }
    resp.context_menu(|ui| {
        ui.set_min_width(170.0);
        if ui.add(egui::Button::new("Rename").shortcut_text("F2")).clicked() {
            action = Some(ListAction::StartRename(row.id.clone()));
            ui.close_menu();
        }
        let duplicate = ui
            .add_enabled(!row.running, egui::Button::new("Duplicate"))
            .on_disabled_hover_text("Wait for the answer to finish");
        if duplicate.clicked() {
            action = Some(ListAction::Duplicate(row.id.clone()));
            ui.close_menu();
        }
        ui.separator();
        if ui.add(egui::Button::new(RichText::new("Delete...").color(Palette::ERR)).shortcut_text("Del")).clicked() {
            action = Some(ListAction::AskDelete(row.id.clone()));
            ui.close_menu();
        }
    });
    action
}

// ---------------------------------------------------------------------------------------------
// Right: N-panel
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct PanelActions {
    use_as_default: bool,
    reset: bool,
    open_write: bool,
}

fn n_panel(app: &mut AppState, ui: &mut Ui) {
    let Some(id) = app.chat.selected.clone() else {
        return;
    };
    let Some(conv) = app.chat.conv(&id) else {
        return;
    };
    let mut system = conv.system.clone();
    let mut params = conv.params.clone();
    let timings = conv.last_timings().cloned();
    let n_ctx = app.status.props.as_ref().map(|p| p.n_ctx).filter(|n| *n > 0);
    let pending_images = app.chat.attachments.len();
    let defaults = app.config.chat_params.clone();
    let mut actions = PanelActions::default();

    egui::ScrollArea::vertical().id_salt("chat-npanel-scroll").auto_shrink([false, false]).show(ui, |ui| {
        widgets::panel(ui, "chat-system", "System prompt", true, |ui| {
            system_panel(ui, &mut system, &mut app.chat.custom_draft, &app.library, &mut actions);
        });
        widgets::panel(ui, "chat-generation", "Generation", true, |ui| {
            generation_panel(ui, &mut params, &defaults, &mut actions);
        });
        widgets::panel(ui, "chat-context", "Context", true, |ui| {
            context_panel(ui, timings.as_ref(), n_ctx, pending_images);
        });
    });

    if actions.reset {
        params = defaults;
    }
    if actions.use_as_default {
        app.config.chat_params = params.clone();
        app.save_config();
        app.toast(ToastKind::Ok, "New chats now start with these settings.");
    }
    if let Some(conv) = app.chat.conv_mut(&id) {
        if conv.system != system || conv.params != params {
            conv.system = system;
            conv.params = params;
            mark_dirty(app, &id);
        }
    }
    if actions.open_write {
        // `show` stops running once Write is shown: save the preset just picked now.
        flush_pending(app);
        app.workspace = Workspace::Write;
    }
}

fn system_panel(
    ui: &mut Ui,
    system: &mut SystemChoice,
    custom_draft: &mut String,
    library: &Library,
    actions: &mut PanelActions,
) {
    let mut kind = system.kind();
    widgets::prop_row(ui, "Preset", |ui| {
        egui::ComboBox::from_id_salt("chat-system-preset")
            .width(ui.available_width())
            .selected_text(kind.label())
            .show_ui(ui, |ui| {
                for k in SystemKind::ALL {
                    ui.selectable_value(&mut kind, k, k.label()).on_hover_text(k.tooltip());
                }
            });
    });
    if kind != system.kind() {
        if let SystemChoice::Custom(text) = &*system {
            custom_draft.clone_from(text);
        }
        *system = kind.choice(custom_draft);
    }
    ui.add_space(2.0);
    match system {
        SystemChoice::None => hint(ui, "No system message: Bonsai answers as a general assistant."),
        SystemChoice::H3R2v | SystemChoice::H3F2v => {
            let mode = if matches!(system, SystemChoice::H3R2v) { Mode::R2v } else { Mode::F2v };
            let words = word_count(library.system_prompt(mode));
            hint(
                ui,
                &format!(
                    "Sends the Library's {} writer prompt ({} words) before the chat.",
                    mode.as_str(),
                    fmt_int(words)
                ),
            );
            hint(ui, "To write prompts, use the Write workspace: it adds the request builder, checks and fixes.");
            if ui.link("Open the Write workspace").clicked() {
                actions.open_write = true;
            }
        }
        SystemChoice::Custom(text) => {
            let edit = egui::TextEdit::multiline(text)
                .id_salt("chat-system-custom")
                .desired_rows(6)
                .desired_width(f32::INFINITY)
                .hint_text("You are a helpful assistant. Answer briefly.");
            if ui.add(edit).changed() {
                custom_draft.clone_from(text);
            }
            hint(ui, &format!("{} words, sent first in every request.", fmt_int(word_count(text))));
        }
    }
}

fn generation_panel(ui: &mut Ui, params: &mut GenParams, defaults: &GenParams, actions: &mut PanelActions) {
    ui.label(RichText::new("Thinking").color(Palette::TEXT_DIM));
    let medium = if ui.available_width() < 250.0 { "Med" } else { "Medium" };
    let options = [
        (ThinkingMode::Off, "Off"),
        (ThinkingMode::Low, "Low"),
        (ThinkingMode::Medium, medium),
        (ThinkingMode::High, "High"),
    ];
    widgets::enum_buttons(ui, &mut params.thinking, &options);
    hint(ui, params.thinking.hint());
    ui.add_space(4.0);
    widgets::prop_row(ui, "Temperature", |ui| slider(ui, &mut params.temperature, 0.0..=2.0, 0.05, 2))
        .on_hover_text("Higher = more varied answers. 0.7 suits chat, 0.4 prompt writing.");
    widgets::prop_row(ui, "Max tokens", |ui| {
        ui.add_sized(
            [ui.available_width(), 20.0],
            egui::DragValue::new(&mut params.max_tokens).range(16..=32_768).speed(16.0),
        )
    })
    .on_hover_text("Longest answer, thinking included.");
    if params.thinking != ThinkingMode::Off && params.max_tokens < 4096 {
        ui.label(
            RichText::new(format!("{} Thinking can use thousands of tokens: raise Max tokens.", icons::WARN))
                .small()
                .color(Palette::WARN),
        );
    }
    ui.add_space(2.0);
    egui::CollapsingHeader::new("Advanced").id_salt("chat-generation-advanced").default_open(false).show(ui, |ui| {
        widgets::prop_row(ui, "Top-p", |ui| slider(ui, &mut params.top_p, 0.0..=1.0, 0.01, 2));
        widgets::prop_row(ui, "Top-k", |ui| {
            ui.add_sized([ui.available_width(), 20.0], egui::DragValue::new(&mut params.top_k).range(0..=500))
        });
        widgets::prop_row(ui, "Min-p", |ui| slider(ui, &mut params.min_p, 0.0..=1.0, 0.01, 2));
        widgets::prop_row(ui, "Seed", |ui| seed_field(ui, &mut params.seed));
    });
    ui.add_space(6.0);
    let differs = params != defaults;
    ui.horizontal(|ui| {
        let w = ((ui.available_width() - ui.spacing().item_spacing.x) / 2.0).max(40.0);
        let use_default = ui
            .add_enabled(differs, egui::Button::new("Use as default").min_size(egui::vec2(w, 22.0)))
            .on_hover_text("New chats start with these settings (also in Settings)")
            .on_disabled_hover_text("These already are the default settings");
        if use_default.clicked() {
            actions.use_as_default = true;
        }
        let reset = ui
            .add_enabled(differs, egui::Button::new("Reset").min_size(egui::vec2(w, 22.0)))
            .on_hover_text("Back to the default chat settings")
            .on_disabled_hover_text("These already are the default settings");
        if reset.clicked() {
            actions.reset = true;
        }
    });
}

fn seed_field(ui: &mut Ui, seed: &mut Option<i64>) -> Response {
    let mut fixed = seed.is_some();
    let resp = ui.checkbox(&mut fixed, "Fixed").on_hover_text("The same seed and messages give the same answer");
    if resp.changed() {
        let value = seed.unwrap_or(42);
        *seed = fixed.then_some(value);
    }
    match seed.as_mut() {
        Some(value) => {
            ui.add_sized([ui.available_width(), 20.0], egui::DragValue::new(value).range(0..=i64::from(u32::MAX)));
        }
        None => {
            ui.label(RichText::new("random").color(Palette::TEXT_FAINT));
        }
    }
    resp
}

fn context_panel(ui: &mut Ui, timings: Option<&Timings>, n_ctx: Option<u32>, pending_images: usize) {
    let used = timings.map(|t| u64::from(t.prompt_n) + u64::from(t.cache_n) + u64::from(t.predicted_n)).unwrap_or(0);
    match n_ctx {
        Some(n) => {
            let fraction = (used as f32 / n as f32).clamp(0.0, 1.0);
            let color = if fraction >= 0.9 {
                Palette::ERR
            } else if fraction >= 0.75 {
                Palette::WARN
            } else {
                Palette::BLUE
            };
            let text = format!("{} / {} tokens  ({:.0} %)", fmt_int(used), fmt_int(u64::from(n)), fraction * 100.0);
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_height(18.0)
                    .corner_radius(CornerRadius::same(3))
                    .fill(color)
                    .text(RichText::new(text).small().color(Palette::TEXT)),
            )
            .on_hover_text("Tokens of the last request (prompt + answer) of the context per slot");
            if fraction >= 0.85 {
                ui.label(
                    RichText::new(format!("{} Nearly full: start a new chat.", icons::WARN))
                        .small()
                        .color(Palette::WARN),
                );
            }
        }
        None => {
            let text = format!("{} tokens used", fmt_int(used));
            ui.add(
                egui::ProgressBar::new(0.0)
                    .desired_height(18.0)
                    .corner_radius(CornerRadius::same(3))
                    .text(RichText::new(text).small().color(Palette::TEXT_DIM)),
            );
            hint(ui, "The context size shows while Bonsai is running.");
        }
    }
    match timings {
        Some(t) => hint(
            ui,
            &format!(
                "Last request: {} prompt tokens ({} cached) + {} generated.",
                fmt_int(u64::from(t.prompt_n) + u64::from(t.cache_n)),
                fmt_int(u64::from(t.cache_n)),
                fmt_int(u64::from(t.predicted_n))
            ),
        ),
        None => hint(ui, "No answer in this chat yet."),
    }
    if pending_images > 0 {
        hint(
            ui,
            &format!(
                "{pending_images} attached picture{}: about +{} tokens.",
                plural(pending_images),
                fmt_int(pending_images as u64 * TOKENS_PER_IMAGE)
            ),
        );
    }
    hint(ui, "Each image ~1,100 tokens. Start a new chat when it is nearly full.");
}

// ---------------------------------------------------------------------------------------------
// Centre: header, messages, input
// ---------------------------------------------------------------------------------------------

enum HeaderAction {
    Regenerate,
    Clear,
    CopyAll,
}

enum CardAction {
    Copy(String),
    Retry,
    RetryWithoutPictures,
    OpenImage(PathBuf),
}

/// The running answer, for its card.
struct Live {
    index: usize,
    phase: String,
    secs: u64,
    /// The answer text just began: fold the thinking section (this frame only).
    fold: bool,
}

struct CardCtx<'a> {
    conv_id: &'a str,
    index: usize,
    live: Option<&'a Live>,
    accent: Color32,
    show_reasoning: bool,
    can_retry: bool,
    /// The chat has pictures: a failed answer also offers "Retry without pictures".
    pictures: bool,
}

fn main_area(app: &mut AppState, ui: &mut Ui) {
    let Some(id) = app.chat.selected.clone() else {
        ui.centered_and_justified(|ui| ui.label(RichText::new("No chat selected").color(Palette::TEXT_DIM)));
        return;
    };
    let action = header(app, ui, &id);
    egui::TopBottomPanel::bottom("chat-input")
        .frame(egui::Frame::new().fill(Palette::HEADER).inner_margin(Margin::symmetric(10, 8)))
        .show_inside(ui, |ui| input_area(app, ui));
    egui::CentralPanel::default().frame(egui::Frame::new().fill(Palette::REGION)).show_inside(ui, |ui| {
        // The input may have created or re-sorted chats: look the selection up again.
        if let Some(current) = app.chat.selected.clone() {
            messages_area(app, ui, &current);
        }
        drop_overlay(ui);
    });
    match action {
        Some(HeaderAction::CopyAll) => {
            if let Some(conv) = app.chat.conv(&id) {
                let count = conv.messages.len();
                ui.ctx().copy_text(conv.transcript());
                app.toast(ToastKind::Ok, format!("Copied the conversation ({count} message{}).", plural(count)));
            }
        }
        Some(HeaderAction::Clear) => app.chat.confirm = Some(Confirm::Clear(id)),
        Some(HeaderAction::Regenerate) => regenerate(app, &id),
        None => {}
    }
}

fn header(app: &AppState, ui: &mut Ui, id: &str) -> Option<HeaderAction> {
    let conv = app.chat.conv(id)?;
    let running_here = app.chat.is_running_in(id);
    let busy = app.chat.running.is_some();
    let has_messages = !conv.messages.is_empty();
    let can_regen = !busy && can_regenerate(conv);
    let badge = conv.system.badge();
    // Narrow area: icon-only buttons, so the title keeps some room.
    let compact = ui.available_width() < 640.0;
    let label = |icon: &str, text: &str| if compact { icon.to_string() } else { format!("{icon} {text}") };
    let buttons = if compact { 96.0 } else { 300.0 };
    // Room for the title: the strip minus its margins, the buttons and the badge.
    let budget = ui.available_width() - 16.0 - buttons - if badge.is_some() { 110.0 } else { 0.0 };
    let title = fit_text(ui, &conv.title, &FontId::proportional(13.5), budget.max(60.0));
    let mut action = None;
    widgets::area_header(ui, &title, |ui| {
        let copy = ui
            .add_enabled(
                has_messages,
                egui::Button::new(label(icons::COPY, "Copy all")).min_size(egui::vec2(24.0, 22.0)),
            )
            .on_hover_text("Copy the whole conversation as text")
            .on_disabled_hover_text("No messages yet");
        if copy.clicked() {
            action = Some(HeaderAction::CopyAll);
        }
        let clear = ui
            .add_enabled(
                has_messages && !running_here,
                egui::Button::new(label(icons::TRASH, "Clear")).min_size(egui::vec2(24.0, 22.0)),
            )
            .on_hover_text("Remove every message of this chat (asks first)")
            .on_disabled_hover_text(if running_here { "Stop the answer first (Esc)" } else { "No messages yet" });
        if clear.clicked() {
            action = Some(HeaderAction::Clear);
        }
        let regen = ui
            .add_enabled(
                can_regen,
                egui::Button::new(label(icons::RESTART, "Regenerate")).min_size(egui::vec2(24.0, 22.0)),
            )
            .on_hover_text("Drop the last answer and ask Bonsai again")
            .on_disabled_hover_text(if busy { "Bonsai is answering" } else { "Nothing to regenerate yet" });
        if regen.clicked() {
            action = Some(HeaderAction::Regenerate);
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            if let Some(badge) = badge {
                widgets::badge(ui, badge, Palette::INFO).on_hover_text("System message of this chat (N-panel)");
            }
        });
    });
    action
}

fn messages_area(app: &mut AppState, ui: &mut Ui, id: &str) {
    let accent = theme::accent(ui.ctx());
    let scroll_to_bottom = std::mem::take(&mut app.chat.scroll_to_bottom);
    let busy = app.chat.running.is_some();
    let answer_started = app
        .chat
        .running
        .as_ref()
        .filter(|r| r.conv_id == id)
        .and_then(|r| app.chat.conv(id).and_then(|c| c.messages.get(r.index)))
        .is_some_and(|e| !e.content.trim().is_empty());
    let live = app.chat.running.as_mut().filter(|r| r.conv_id == id).map(|r| {
        let fold = answer_started && !r.folded;
        r.folded |= fold;
        Live { index: r.index, phase: r.phase.clone(), secs: (app.time - r.started).max(0.0) as u64, fold }
    });
    let show_reasoning = app.config.show_reasoning;
    let ready = app.bonsai_ready();
    let auto_start = app.config.auto_start;
    let Some(conv) = app.chat.conv(id) else {
        return;
    };
    let system_text = conv.system.text(&app.library);
    let pictures = conv.messages.iter().any(|m| !m.images.is_empty());
    let mut actions = Vec::new();
    egui::ScrollArea::vertical().id_salt(("chat-messages", id)).stick_to_bottom(true).auto_shrink([false, false]).show(
        ui,
        |ui| {
            centered_column(ui, COLUMN_MAX, |ui| {
                ui.add_space(12.0);
                if let (Some(text), Some(label)) = (system_text, conv.system.badge()) {
                    system_card(ui, id, label, text);
                    ui.add_space(8.0);
                }
                if conv.messages.is_empty() {
                    empty_state(ui, ready, auto_start);
                }
                let last = conv.messages.len().saturating_sub(1);
                for (index, entry) in conv.messages.iter().enumerate() {
                    let card = CardCtx {
                        conv_id: id,
                        index,
                        live: live.as_ref().filter(|l| l.index == index),
                        accent,
                        show_reasoning,
                        can_retry: index == last && !busy,
                        pictures,
                    };
                    actions.extend(message_card(ui, entry, &card));
                    ui.add_space(8.0);
                }
                ui.add_space(4.0);
            });
            if scroll_to_bottom {
                ui.scroll_to_cursor(Some(Align::BOTTOM));
            }
        },
    );
    for action in actions {
        match action {
            CardAction::Copy(text) => {
                ui.ctx().copy_text(text);
                app.toast(ToastKind::Ok, "Copied to the clipboard.");
            }
            CardAction::Retry => regenerate(app, id),
            CardAction::RetryWithoutPictures => retry_without_pictures(app, id),
            CardAction::OpenImage(path) => {
                if let Err(e) = util::open_path(&path) {
                    app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", path.display()));
                }
            }
        }
    }
}

/// Lay out `add` in a column at most `max_width` wide, centred in the available width.
fn centered_column<R>(ui: &mut Ui, max_width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let available = ui.available_width();
    let width = (available - 28.0).clamp(120.0, max_width);
    let pad = ((available - width) / 2.0).max(0.0);
    ui.horizontal_top(|ui| {
        ui.add_space(pad);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui)
        })
        .inner
    })
    .inner
}

fn empty_state(ui: &mut Ui, ready: bool, auto_start: bool) {
    ui.add_space(56.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(icons::CHAT).size(30.0).color(Palette::TEXT_FAINT));
        ui.add_space(6.0);
        ui.label(RichText::new("Chat with Bonsai").size(17.0).color(Palette::TEXT));
        ui.add_space(2.0);
        ui.label(
            RichText::new(
                "Ask anything, attach pictures for its vision input, or pick an H3 system prompt on the right.",
            )
            .color(Palette::TEXT_DIM),
        );
        ui.label(
            RichText::new("Enter sends  ·  Shift+Enter adds a new line  ·  Esc stops the answer")
                .small()
                .color(Palette::TEXT_FAINT),
        );
        if !ready {
            ui.add_space(10.0);
            let (text, color) = if auto_start {
                ("Bonsai is not running: it starts by itself with the first message.", Palette::TEXT_DIM)
            } else {
                ("Bonsai is not running and auto-start is off: start it in the Bonsai workspace.", Palette::WARN)
            };
            ui.label(RichText::new(text).small().color(color));
        }
    });
}

fn card_frame(fill: Color32) -> egui::Frame {
    egui::Frame::new().fill(fill).corner_radius(CornerRadius::same(4)).inner_margin(Margin {
        left: 14,
        right: 10,
        top: 7,
        bottom: 6,
    })
}

/// The coloured role strip on the left edge of a card.
fn paint_strip(ui: &Ui, rect: Rect, color: Color32) {
    let strip = Rect::from_min_max(rect.min, egui::pos2(rect.min.x + 3.0, rect.max.y));
    ui.painter().rect_filled(strip, CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 }, color);
}

fn system_card(ui: &mut Ui, conv_id: &str, label: &str, text: &str) {
    let frame = card_frame(CARD_SYSTEM).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let title = RichText::new(format!("System  ·  {label}  ·  {} words", fmt_int(word_count(text))))
            .color(Palette::TEXT_DIM);
        egui::CollapsingHeader::new(title).id_salt(("chat-system-card", conv_id)).default_open(false).show(ui, |ui| {
            ui.add(
                Label::new(RichText::new(text.trim()).font(theme::mono(12.0)).color(Palette::TEXT_DIM))
                    .selectable(true)
                    .wrap(),
            );
        });
    });
    paint_strip(ui, frame.response.rect, Palette::TEXT_FAINT);
}

fn message_card(ui: &mut Ui, entry: &ChatEntry, card: &CardCtx<'_>) -> Option<CardAction> {
    let failed = entry.error.is_some();
    let (strip, name, name_color, fill) = match entry.role {
        _ if failed => (Palette::ERR, "Bonsai", Palette::ERR, CARD_ERROR),
        Role::User => (Palette::BLUE, "You", Palette::INFO, CARD_USER),
        Role::Assistant => (card.accent, "Bonsai", card.accent, CARD_ASSISTANT),
        Role::System => (Palette::TEXT_FAINT, "System", Palette::TEXT_DIM, CARD_SYSTEM),
    };
    let mut action = None;
    let frame = card_frame(fill).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 5.0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).strong().color(name_color));
            if let Some(live) = card.live {
                ui.add(egui::Spinner::new().size(11.0));
                ui.label(
                    RichText::new(format!("{}  {}", live.phase, widgets::fmt_secs(live.secs)))
                        .small()
                        .color(Palette::TEXT_DIM),
                );
            }
        });
        if card.show_reasoning && !entry.reasoning.trim().is_empty() {
            // Open while Bonsai thinks; folded once when the answer starts (the user may reopen it).
            let force_open = card.live.and_then(|live| {
                if entry.content.trim().is_empty() {
                    Some(true)
                } else {
                    live.fold.then_some(false)
                }
            });
            reasoning_section(ui, Id::new(("chat-think", card.conv_id, card.index)), &entry.reasoning, force_open);
        }
        if !entry.content.trim().is_empty() {
            content_view(ui, &entry.content, &mut action);
        } else if card.live.is_some() {
            if entry.reasoning.trim().is_empty() || !card.show_reasoning {
                ui.label(RichText::new("Waiting for the first words...").italics().color(Palette::TEXT_DIM));
            }
        } else if entry.stopped {
            ui.label(RichText::new("Stopped before Bonsai answered.").italics().color(Palette::TEXT_DIM));
        } else if !failed && entry.role == Role::Assistant {
            let text = if entry.finish_reason == "length" {
                "No answer text: the thinking used every token. Raise Max tokens or lower Thinking, then Regenerate."
            } else {
                "No answer text."
            };
            ui.label(RichText::new(text).italics().color(Palette::WARN));
        }
        if let Some(error) = &entry.error {
            ui.add(Label::new(RichText::new(error).color(Palette::ERR)).selectable(true).wrap());
        }
        if !entry.images.is_empty() {
            if let Some(path) = thumbnails(ui, &entry.images, 72.0) {
                action = Some(CardAction::OpenImage(path));
            }
        }
        if card.live.is_none() {
            footer(ui, entry, card, &mut action);
        }
    });
    paint_strip(ui, frame.response.rect, strip);
    action
}

fn reasoning_section(ui: &mut Ui, id: Id, reasoning: &str, force_open: Option<bool>) {
    let words = word_count(reasoning);
    let title = RichText::new(format!("{} Thinking ({} words)", icons::THINK, fmt_int(words))).color(Palette::TEXT_DIM);
    let mut header = egui::CollapsingHeader::new(title).id_salt(id).default_open(false);
    if force_open.is_some() {
        header = header.open(force_open);
    }
    header.show(ui, |ui| {
        egui::Frame::new()
            .fill(THINK_FILL)
            .corner_radius(CornerRadius::same(3))
            .inner_margin(Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    Label::new(RichText::new(reasoning.trim()).font(theme::mono(12.0)).color(Palette::TEXT_DIM))
                        .selectable(true)
                        .wrap(),
                );
            });
    });
}

/// A piece of an answer: plain text or a fenced code block.
enum Segment<'a> {
    Text(&'a str),
    Code { lang: &'a str, code: &'a str },
}

/// Byte offset of the first line that starts with a ``` fence (leading spaces allowed).
fn find_fence(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            return Some(offset + (line.len() - trimmed.len()));
        }
        offset += line.len();
    }
    None
}

/// Split an answer into text and ``` code blocks (an unclosed block runs to the end, as while
/// streaming).
fn split_code_blocks(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = find_fence(rest) {
        let before = rest[..start].trim();
        if !before.is_empty() {
            out.push(Segment::Text(before));
        }
        let after = &rest[start + 3..];
        let (lang, body) = match after.find('\n') {
            Some(nl) => (after[..nl].trim(), &after[nl + 1..]),
            None => (after.trim(), ""),
        };
        match find_fence(body) {
            Some(end) => {
                out.push(Segment::Code { lang, code: body[..end].trim_end_matches(['\n', '\r']) });
                let tail = &body[end + 3..];
                rest = tail.find('\n').map_or("", |nl| &tail[nl + 1..]);
            }
            None => {
                out.push(Segment::Code { lang, code: body.trim_end_matches(['\n', '\r']) });
                rest = "";
            }
        }
    }
    let tail = rest.trim();
    if !tail.is_empty() {
        out.push(Segment::Text(tail));
    }
    out
}

fn content_view(ui: &mut Ui, text: &str, action: &mut Option<CardAction>) {
    for segment in split_code_blocks(text) {
        match segment {
            Segment::Text(text) => {
                ui.add(Label::new(RichText::new(text).color(Palette::TEXT)).selectable(true).wrap());
            }
            Segment::Code { lang, code } => {
                egui::Frame::new()
                    .fill(Palette::INPUT)
                    .corner_radius(CornerRadius::same(3))
                    .inner_margin(Margin::symmetric(8, 5))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let name = if lang.is_empty() { "text" } else { lang };
                            ui.label(RichText::new(name).small().color(Palette::TEXT_FAINT));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if flat_icon(ui, icons::COPY, "Copy this block").clicked() {
                                    *action = Some(CardAction::Copy(code.to_string()));
                                }
                            });
                        });
                        ui.add(
                            Label::new(RichText::new(code).monospace().color(Palette::TEXT)).selectable(true).wrap(),
                        );
                    });
            }
        }
    }
}

/// Picture thumbnails of a message; returns the one clicked.
fn thumbnails(ui: &mut Ui, images: &[PathBuf], max_height: f32) -> Option<PathBuf> {
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        for path in images {
            let name = file_name(path);
            let exists = path.is_file();
            if exists && has_ext(path, &PREVIEW_EXTS) {
                // Exact box: in a horizontal layout the default fit would shrink it to the row height.
                let image = egui::Image::new(util::file_uri(path))
                    .fit_to_exact_size(egui::vec2(max_height * 1.6, max_height))
                    .corner_radius(CornerRadius::same(3))
                    .sense(Sense::click());
                let resp = ui.add(image).on_hover_text(format!("{name}\nClick to open"));
                if resp.clicked() {
                    clicked = Some(path.clone());
                }
            } else {
                let (text, color) = if exists {
                    (format!("{} {name}", icons::IMAGE), Palette::TEXT_DIM)
                } else {
                    (format!("{} {name} (missing)", icons::IMAGE), Palette::ERR)
                };
                egui::Frame::new()
                    .fill(Palette::INPUT)
                    .corner_radius(CornerRadius::same(3))
                    .inner_margin(Margin::symmetric(8, 6))
                    .show(ui, |ui| ui.label(RichText::new(text).small().color(color)))
                    .response
                    .on_hover_text(path.display().to_string());
            }
        }
    });
    clicked
}

fn footer(ui: &mut Ui, entry: &ChatEntry, card: &CardCtx<'_>, action: &mut Option<CardAction>) {
    ui.horizontal(|ui| {
        let (text, details) = footer_text(entry);
        if !text.is_empty() {
            let resp = ui.label(RichText::new(text).small().color(Palette::TEXT_FAINT));
            if let Some(details) = details {
                resp.on_hover_text(details);
            }
        }
        if entry.stopped {
            ui.label(RichText::new("(stopped)").small().color(Palette::WARN)).on_hover_text("You stopped this answer");
        }
        if entry.finish_reason == "length" && !entry.content.trim().is_empty() {
            ui.label(RichText::new("cut at the token limit").small().color(Palette::WARN))
                .on_hover_text("The answer reached Max tokens (Generation panel)");
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if !entry.content.trim().is_empty() && flat_icon(ui, icons::COPY, "Copy this message").clicked() {
                *action = Some(CardAction::Copy(entry.content.trim().to_string()));
            }
            if entry.error.is_some() && card.can_retry {
                if ui.small_button(format!("{} Retry", icons::RESTART)).on_hover_text("Ask Bonsai again").clicked() {
                    *action = Some(CardAction::Retry);
                }
                let tip = "Take the pictures out of this chat (the files stay) and ask again: for a Bonsai \
                           without vision, or pictures that do not fit in its context";
                if card.pictures && ui.small_button("Retry without pictures").on_hover_text(tip).clicked() {
                    *action = Some(CardAction::RetryWithoutPictures);
                }
            }
        });
    });
}

/// Footer text of a card and the details for its tooltip.
fn footer_text(entry: &ChatEntry) -> (String, Option<String>) {
    match entry.role {
        Role::User | Role::System => {
            let words = word_count(&entry.content);
            let mut text = format!("{} word{}", fmt_int(words), plural(words as usize));
            if !entry.images.is_empty() {
                let n = entry.images.len();
                text.push_str(&format!("  ·  {n} picture{}", plural(n)));
            }
            (text, None)
        }
        Role::Assistant => match &entry.timings {
            Some(t) => {
                let secs = if entry.elapsed_ms > 0 {
                    entry.elapsed_ms as f64 / 1000.0
                } else {
                    (t.prompt_ms + t.predicted_ms) / 1000.0
                };
                let text = format!(
                    "{} tokens  ·  {:.1} tok/s  ·  {}",
                    fmt_int(u64::from(t.predicted_n)),
                    t.predicted_per_second,
                    fmt_seconds(secs)
                );
                let details = format!(
                    "Prompt: {} new + {} cached tokens, {:.0} tok/s ({})\nAnswer: {} tokens, {:.1} tok/s ({})",
                    fmt_int(u64::from(t.prompt_n)),
                    fmt_int(u64::from(t.cache_n)),
                    t.prompt_per_second,
                    fmt_seconds(t.prompt_ms / 1000.0),
                    fmt_int(u64::from(t.predicted_n)),
                    t.predicted_per_second,
                    fmt_seconds(t.predicted_ms / 1000.0)
                );
                (text, Some(details))
            }
            None => (String::new(), None),
        },
    }
}

fn input_area(app: &mut AppState, ui: &mut Ui) {
    if !app.chat.attachments.is_empty() {
        if let Some(index) = attachment_strip(ui, &app.chat.attachments) {
            app.chat.attachments.remove(index);
        }
        let n = app.chat.attachments.len();
        let mut note =
            format!("{n} picture{} attached  ·  about +{} tokens", plural(n), fmt_int(n as u64 * TOKENS_PER_IMAGE));
        let no_vision = app.status.props.as_ref().is_some_and(|p| !p.vision);
        if no_vision {
            note.push_str("  ·  the running Bonsai has no vision projector: pictures will fail");
        }
        if n > 0 {
            ui.label(RichText::new(note).small().color(if no_vision { Palette::WARN } else { Palette::TEXT_DIM }));
        }
        ui.add_space(4.0);
    }

    let input_id = Id::new(INPUT_ID);
    // Enter sends: take the key before the text box sees it, so no new line is added. Not while
    // the confirmation popup is open (there Enter confirms), and the Enter that commits an IME
    // composition is only taken.
    let mut send = app.chat.confirm.is_none()
        && ui.memory(|m| m.has_focus(input_id))
        && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(Modifiers::NONE, Key::Enter))
        && !app.chat.ime_enter;
    egui::ScrollArea::vertical()
        .id_salt("chat-input-scroll")
        .max_height(INPUT_MAX_HEIGHT)
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            let edit = egui::TextEdit::multiline(&mut app.chat.input)
                .id(input_id)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text(INPUT_HINT)
                .background_color(Palette::INPUT)
                .margin(Margin::symmetric(6, 5));
            let resp = ui.add(edit);
            if app.chat.focus_input {
                resp.request_focus();
                app.chat.focus_input = false;
            }
        });
    ui.add_space(6.0);

    let selected = app.chat.selected.clone().unwrap_or_default();
    let running = app.chat.running.as_ref().map(|r| {
        let elsewhere =
            (r.conv_id != selected).then(|| app.chat.conv(&r.conv_id).map(|c| c.title.clone()).unwrap_or_default());
        (r.phase.clone(), (app.time - r.started).max(0.0) as u64, elsewhere)
    });
    let can_send = running.is_none() && (!app.chat.input.trim().is_empty() || !app.chat.attachments.is_empty());
    let params = app.chat.conv(&selected).map(|c| c.params.clone()).unwrap_or_else(GenParams::chat);
    let ready = app.bonsai_ready();
    let auto_start = app.config.auto_start;
    let mut attach = false;
    let mut stop = false;
    ui.horizontal(|ui| {
        let attach_button = ui
            .add(egui::Button::new(format!("{} Attach images", icons::IMAGE)).min_size(egui::vec2(0.0, 28.0)))
            .on_hover_text(ATTACH_TOOLTIP);
        if attach_button.clicked() {
            attach = true;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if running.is_some() {
                let resp = ui
                    .add(egui::Button::new(format!("{} Stop", icons::STOP)).min_size(egui::vec2(110.0, 28.0)))
                    .on_hover_text("Stop the answer (Esc)");
                if resp.clicked() {
                    stop = true;
                }
            } else {
                let resp = ui
                    .allocate_ui_with_layout(egui::vec2(110.0, 30.0), Layout::top_down(Align::Min), |ui| {
                        widgets::primary_button(ui, "Send", can_send)
                    })
                    .inner
                    .on_hover_text("Send the message (Enter or Ctrl+Enter)")
                    .on_disabled_hover_text("Type a message or attach a picture first");
                if resp.clicked() {
                    send = true;
                }
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.add_space(4.0);
                match &running {
                    Some((phase, secs, elsewhere)) => {
                        ui.add(egui::Spinner::new().size(13.0));
                        let mut text = format!("{phase}  {}", widgets::fmt_secs(*secs));
                        if let Some(title) = elsewhere {
                            text.push_str(&format!("  ·  in \u{201c}{title}\u{201d}"));
                        }
                        ui.add(Label::new(RichText::new(text).color(Palette::TEXT_DIM)).truncate());
                    }
                    None if !ready && !auto_start => {
                        ui.add(
                            Label::new(
                                RichText::new(format!(
                                    "{} Bonsai is not running and auto-start is off (Bonsai workspace)",
                                    icons::WARN
                                ))
                                .small()
                                .color(Palette::WARN),
                            )
                            .truncate(),
                        );
                    }
                    None => {
                        let mut text = format!(
                            "Thinking {}  ·  temperature {:.2}  ·  max {} tokens",
                            params.thinking.as_str(),
                            params.temperature,
                            fmt_int(u64::from(params.max_tokens))
                        );
                        if !ready {
                            text.push_str("  ·  Bonsai starts when you send");
                        }
                        ui.add(Label::new(RichText::new(text).small().color(Palette::TEXT_FAINT)).truncate());
                    }
                }
            });
        });
    });

    if attach {
        for path in util::pick_images("Attach images") {
            add_attachment(app, path);
        }
    }
    if stop {
        stop_running(app);
    }
    if send && can_send {
        send_message(app);
    }
}

/// Thumbnails of the pictures waiting to be sent, each with a remove button; returns the index
/// to remove.
fn attachment_strip(ui: &mut Ui, paths: &[PathBuf]) -> Option<usize> {
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (index, path) in paths.iter().enumerate() {
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(72.0, 54.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(3), Palette::INPUT);
            let inner = rect.shrink(2.0);
            let mut drawn = false;
            if has_ext(path, &PREVIEW_EXTS) && path.is_file() {
                let image = egui::Image::new(util::file_uri(path)).corner_radius(CornerRadius::same(2));
                if let Some(size) = image.load_and_calc_size(ui, inner.size()) {
                    image.paint_at(ui, Rect::from_center_size(inner.center(), size));
                    drawn = true;
                }
            }
            if !drawn {
                ui.painter().text(
                    inner.center(),
                    Align2::CENTER_CENTER,
                    icons::IMAGE,
                    FontId::proportional(18.0),
                    Palette::TEXT_FAINT,
                );
            }
            resp.on_hover_text(path.display().to_string());
            let x_rect = Rect::from_min_size(egui::pos2(rect.right() - 18.0, rect.top() + 2.0), egui::vec2(16.0, 16.0));
            let x = ui.interact(x_rect, ui.id().with(("chat-attachment-remove", index)), Sense::click());
            let x_fill = if x.hovered() { DANGER } else { Color32::from_black_alpha(170) };
            ui.painter().circle_filled(x_rect.center(), 7.5, x_fill);
            ui.painter().text(x_rect.center(), Align2::CENTER_CENTER, "✖", FontId::proportional(9.0), Color32::WHITE);
            if x.on_hover_text("Remove this picture").clicked() {
                remove = Some(index);
            }
        }
    });
    remove
}

/// Dim the message area while files are dragged over the window.
fn drop_overlay(ui: &Ui) {
    if ui.ctx().input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let rect = ui.max_rect();
    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("chat-drop-overlay")));
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
    painter.rect_stroke(
        rect.shrink(10.0),
        CornerRadius::same(6),
        Stroke::new(2.0_f32, Palette::BLUE),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        "Drop pictures to attach them",
        FontId::proportional(18.0),
        Palette::TEXT,
    );
}

// ---------------------------------------------------------------------------------------------
// Confirmation popup (also used by the History workspace)
// ---------------------------------------------------------------------------------------------

fn confirm_dialog(app: &mut AppState, ctx: &egui::Context) {
    let Some(confirm) = app.chat.confirm.clone() else {
        return;
    };
    let (id, title, body, label) = match &confirm {
        Confirm::Delete(id) => {
            let (name, count) = app.chat.conv(id).map(|c| (c.title.clone(), c.messages.len())).unwrap_or_default();
            let body = if count == 0 {
                "This chat has no messages.".to_string()
            } else {
                format!(
                    "Its {count} message{} are removed from the chats folder. This cannot be undone.",
                    plural(count)
                )
            };
            (id, format!("Delete \u{201c}{}\u{201d}?", ellipsize(&name, 48)), body, "Delete")
        }
        Confirm::Clear(id) => {
            let (name, count) = app.chat.conv(id).map(|c| (c.title.clone(), c.messages.len())).unwrap_or_default();
            let body = format!(
                "All {count} message{} are removed; the title and settings stay. This cannot be undone.",
                plural(count)
            );
            (id, format!("Clear \u{201c}{}\u{201d}?", ellipsize(&name, 48)), body, "Clear")
        }
    };
    match confirm_popup(ctx, "chat-confirm", &title, &body, label) {
        Some(true) => {
            app.chat.confirm = None;
            match &confirm {
                Confirm::Delete(_) => delete_conversation(app, id),
                Confirm::Clear(_) => clear_conversation(app, id),
            }
        }
        Some(false) => app.chat.confirm = None,
        None => {}
    }
}

/// Blender-like confirmation popup with a red action button. `Some(true)`: confirmed (button or
/// Enter), `Some(false)`: cancelled (button, Esc or a click outside), `None`: still open.
pub(super) fn confirm_popup(ctx: &egui::Context, id: &str, title: &str, body: &str, action: &str) -> Option<bool> {
    let mut result = None;
    // When it opens (it was not drawn in the previous pass), a text field behind it must not keep
    // the keyboard (Enter belongs here).
    let open_key = Id::new(id).with("bh3-confirm-open");
    let pass = ctx.cumulative_pass_nr();
    if ctx.data(|d| d.get_temp::<u64>(open_key)).is_none_or(|last| last + 1 < pass) {
        ctx.memory_mut(|m| m.stop_text_input());
    }
    ctx.data_mut(|d| d.insert_temp(open_key, pass));
    let frame = egui::Frame::new()
        .fill(Palette::HEADER)
        .stroke(Stroke::new(1.0_f32, Palette::OUTLINE))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::same(14));
    let modal = egui::Modal::new(Id::new(id)).frame(frame).show(ctx, |ui| {
        ui.set_width(360.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(icons::WARN).size(17.0).color(Palette::WARN));
            ui.add(Label::new(RichText::new(title).strong().size(14.5).color(Palette::TEXT)).truncate());
        });
        ui.add_space(4.0);
        ui.add(Label::new(RichText::new(body).color(Palette::TEXT_DIM)).wrap());
        ui.add_space(12.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let confirm = egui::Button::new(RichText::new(action).strong().color(Color32::WHITE))
                .fill(DANGER)
                .min_size(egui::vec2(96.0, 26.0));
            if ui.add(confirm).on_hover_text("Enter").clicked() {
                result = Some(true);
            }
            if ui.add(egui::Button::new("Cancel").min_size(egui::vec2(84.0, 26.0))).on_hover_text("Esc").clicked() {
                result = Some(false);
            }
        });
    });
    if result.is_none() {
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            result = Some(true);
        } else if modal.should_close() {
            result = Some(false);
        }
    }
    if result.is_some() {
        ctx.data_mut(|d| d.remove::<u64>(open_key));
    }
    result
}

/// Whether a workspace comes back after another one was shown: `show` is only called while its
/// workspace is visible, so a gap since `last_pass` (the egui pass it last ran in, updated here)
/// means it was hidden. Popups and inline edits left open there are dropped by the caller.
pub(super) fn came_back(ctx: &egui::Context, last_pass: &mut Option<u64>) -> bool {
    let pass = ctx.cumulative_pass_nr();
    let back = last_pass.is_some_and(|last| last + 1 < pass);
    *last_pass = Some(pass);
    back
}

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

fn side_frame() -> egui::Frame {
    egui::Frame::new().fill(Palette::SIDE).inner_margin(Margin::symmetric(8, 8))
}

/// Small dim wrapped text under a setting.
fn hint(ui: &mut Ui, text: &str) {
    ui.add(Label::new(RichText::new(text).small().color(Palette::TEXT_DIM)).wrap());
}

/// Slider with a filled rail (Blender factor field) that uses the row's remaining width.
fn slider(ui: &mut Ui, value: &mut f32, range: RangeInclusive<f32>, step: f64, decimals: usize) -> Response {
    ui.spacing_mut().slider_width = (ui.available_width() - 56.0).max(40.0);
    ui.add(egui::Slider::new(value, range).step_by(step).fixed_decimals(decimals).trailing_fill(true))
}

/// Frameless icon button that lights up on hover.
fn flat_icon(ui: &mut Ui, icon: &str, tooltip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(20.0, 18.0), Sense::click());
    let color = if resp.hovered() { Palette::TEXT } else { Palette::TEXT_DIM };
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(3), Color32::from_gray(66));
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(12.0), color);
    resp.on_hover_text(tooltip)
}

/// One line of text cut with `…` to `max_width`.
fn single_line(ui: &Ui, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    ui.fonts(|f| f.layout_job(job))
}

/// `text`, shortened with `…` so it fits `max_width` in `font`.
fn fit_text(ui: &Ui, text: &str, font: &FontId, max_width: f32) -> String {
    let width = |s: &str| ui.fonts(|f| f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x);
    if width(text) <= max_width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut low, mut high) = (0usize, chars.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        let candidate: String = chars[..mid].iter().chain(std::iter::once(&'…')).collect();
        if width(&candidate) <= max_width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut out: String = chars[..low].iter().collect::<String>().trim_end().to_string();
    out.push('…');
    out
}

/// `text` cut to `max` characters with `…`.
fn ellipsize(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
        None => text.to_string(),
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "You",
        Role::Assistant => "Bonsai",
        Role::System => "System",
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn word_count(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// `12345` -> `12,345`.
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

/// `0.8 s`, `6.8 s`, `42 s`, `1m 12s`.
fn fmt_seconds(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        "-".to_string()
    } else if secs < 10.0 {
        format!("{secs:.1} s")
    } else if secs < 60.0 {
        format!("{secs:.0} s")
    } else {
        widgets::fmt_secs(secs.round() as u64)
    }
}

/// `Today 14:02`, `Yesterday 09:12`, `Mon 18:40`, `12 Oct 10:05`, `2025-12-01`.
fn fmt_when(rfc3339: &str) -> String {
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
        return String::new();
    };
    let time = time.with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    let days = (now.date_naive() - time.date_naive()).num_days();
    match days {
        0 => format!("Today {}", time.format("%H:%M")),
        1 => format!("Yesterday {}", time.format("%H:%M")),
        2..=6 => time.format("%a %H:%M").to_string(),
        _ if time.year() == now.year() => time.format("%d %b %H:%M").to_string(),
        _ => time.format("%Y-%m-%d").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_are_split_out() {
        let text = "Here:\n```json\n{\"a\": 1}\n```\nDone.";
        let parts = split_code_blocks(text);
        assert_eq!(parts.len(), 3);
        assert!(matches!(parts[0], Segment::Text("Here:")));
        assert!(matches!(parts[1], Segment::Code { lang: "json", code: "{\"a\": 1}" }));
        assert!(matches!(parts[2], Segment::Text("Done.")));
        // Unclosed (still streaming): the rest is code.
        let parts = split_code_blocks("a\n```\nlet x = 1;");
        assert!(matches!(parts[1], Segment::Code { lang: "", code: "let x = 1;" }));
    }

    #[test]
    fn request_skips_failed_answers_and_keeps_roles_alternating() {
        let conv = Conversation {
            system: SystemChoice::Custom("Be brief.".into()),
            messages: vec![
                ChatEntry::user("one".into(), Vec::new()),
                ChatEntry { role: Role::Assistant, error: Some("down".into()), ..ChatEntry::default() },
                ChatEntry::user("two".into(), Vec::new()),
                ChatEntry { role: Role::Assistant, content: "answer".into(), ..ChatEntry::default() },
                ChatEntry::user("three".into(), Vec::new()),
            ],
            ..Conversation::default()
        };
        let (messages, problems) = build_request(&conv, &Library::builtin());
        assert!(problems.is_empty());
        let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::System, Role::User, Role::Assistant, Role::User]);
        assert_eq!(messages[1].content, "one\n\ntwo");
        assert!(can_regenerate(&conv));
    }

    #[test]
    fn retry_without_pictures_sends_none_of_them_again() {
        let missing = std::env::temp_dir().join(format!("bh3-chat-test-missing-{}.png", std::process::id()));
        let mut conv = Conversation {
            messages: vec![
                ChatEntry::user("look".into(), vec![missing.clone()]),
                ChatEntry {
                    role: Role::Assistant,
                    error: Some("image input is not supported".into()),
                    ..ChatEntry::default()
                },
                ChatEntry::user("and now?".into(), vec![missing]),
                ChatEntry {
                    role: Role::Assistant,
                    error: Some("image input is not supported".into()),
                    ..ChatEntry::default()
                },
            ],
            ..Conversation::default()
        };
        // Every request reads the pictures of every question again.
        assert_eq!(build_request(&conv, &Library::builtin()).1.len(), 2);
        assert_eq!(drop_pictures(&mut conv), 2);
        let (messages, problems) = build_request(&conv, &Library::builtin());
        assert!(problems.is_empty());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "look\n\nand now?");
        assert!(messages[0].images.is_empty());
        assert!(can_regenerate(&conv));
    }

    #[test]
    fn saves_leave_out_the_answer_still_streaming() {
        let conv = Conversation {
            messages: vec![
                ChatEntry::user("hi".into(), Vec::new()),
                ChatEntry { role: Role::Assistant, content: "Hel".into(), ..ChatEntry::default() },
            ],
            ..Conversation::default()
        };
        assert_eq!(without_streaming(&conv, Some(1)).messages, conv.messages[..1]);
        assert_eq!(without_streaming(&conv, None).messages, conv.messages);
        assert_eq!(without_streaming(&conv, Some(5)).messages, conv.messages);
    }

    #[test]
    fn the_enter_that_commits_an_ime_composition_is_not_a_send() {
        use egui::{Event as E, ImeEvent};
        let enter =
            E::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        // Typing: the composition opens with its first preedit text.
        let (composing, seen) = ime_step(false, &[E::Ime(ImeEvent::Enabled), E::Ime(ImeEvent::Preedit("k".into()))]);
        assert!(composing && seen);
        // Enter before the commit arrives (a later frame): still composing.
        assert_eq!(ime_step(composing, std::slice::from_ref(&enter)), (true, false));
        // Enter with the commit in the same frame: closed, but the frame had IME events.
        let commit = [enter.clone(), E::Ime(ImeEvent::Commit("漢字".into())), E::Ime(ImeEvent::Disabled)];
        assert_eq!(ime_step(true, &commit), (false, true));
        // A plain Enter afterwards sends.
        assert_eq!(ime_step(false, &[enter]), (false, false));
        // An emptied composition (Backspace) is closed; `Enabled` alone (macOS) opens nothing.
        assert_eq!(ime_step(true, &[E::Ime(ImeEvent::Preedit(String::new()))]), (false, true));
        assert_eq!(ime_step(false, &[E::Ime(ImeEvent::Enabled)]), (false, true));
    }

    #[test]
    fn a_workspace_shown_again_after_a_gap_comes_back() {
        let ctx = egui::Context::default();
        let mut last = None;
        let mut back = Vec::new();
        for shown in [true, true, false, false, true, true] {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                if shown {
                    back.push(came_back(ctx, &mut last));
                }
            });
        }
        assert_eq!(back, [false, false, true, false]);
    }

    #[test]
    fn conversation_round_trips_and_tolerates_missing_fields() {
        let dir = std::env::temp_dir().join(format!("bh3-chat-test-{}", std::process::id()));
        let conv = Conversation {
            id: "20261028-101010-001".into(),
            title: "Test".into(),
            system: SystemChoice::H3F2v,
            messages: vec![ChatEntry::user("hi".into(), vec![PathBuf::from("a.png")])],
            ..Conversation::default()
        };
        save_conversation(&dir, &conv).unwrap();
        std::fs::write(
            dir.join("20261028-101011-001.json"),
            r#"{"title": "Old", "messages": [{"role": "user", "content": "x"}]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("broken.json"), "{").unwrap();
        let (list, failed) = load_conversations(&dir);
        assert_eq!(failed, 1);
        assert_eq!(list.len(), 2);
        let loaded = list.iter().find(|c| c.id == conv.id).unwrap();
        assert_eq!(loaded.messages, conv.messages);
        assert_eq!(loaded.system, SystemChoice::H3F2v);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn numbers_are_grouped() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(1100), "1,100");
        assert_eq!(fmt_int(1234567), "1,234,567");
        assert_eq!(ellipsize("abcdef", 3), "abc…");
    }
}
