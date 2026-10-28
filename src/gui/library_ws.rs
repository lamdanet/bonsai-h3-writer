//! Library workspace: edit system prompts, templates, fixes, checklist, guides.
//!
//! Left: every library item with badges (sent to Bonsai / edited / unsaved). Centre: the editor
//! with Save (Ctrl+S), Revert, Reset to built-in, Copy and Count tokens. Right N-panel: what the
//! text is for and where its override file lives, a live preview of the (unsaved) text as the app
//! parses it, and tips.
//!
//! Edits live in per-item buffers until they are saved, so switching items keeps them. Buffers
//! without edits follow `app.library`. While the workspace is shown, the override files are
//! checked every few seconds: when the CLI, another window or an editor changed them,
//! `app.library` is reloaded, and an item with unsaved edits asks "Load the disk version" /
//! "Keep mine" instead of being overwritten either way.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bh3::gen::Mode;
use bh3::library::{self, Library, LibraryItem, Template};
use bh3::service::{Command, Event, JobId};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Key, KeyboardShortcut, Layout, Margin, Modifiers, RichText, Sense, Stroke,
    StrokeKind, Ui, UiBuilder,
};

use super::app::{AppState, ToastKind};
use super::theme::{self, Palette};
use super::util;
use super::widgets::{self, icons};

/// Active row of the item list (Blender's active list item).
const ROW_SELECTED: Color32 = Color32::from_rgb(51, 77, 128);
const ROW_HOVER: Color32 = Color32::from_rgb(48, 48, 48);
/// "edited" badge: Blender's library-override teal.
const EDITED: Color32 = Color32::from_rgb(80, 190, 190);
/// Mode badges in the templates preview.
const R2V_COLOR: Color32 = Palette::INFO;
const F2V_COLOR: Color32 = Color32::from_rgb(190, 140, 235);
/// Saving the shown item.
const SAVE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
/// A token count without an answer by then is given up (a failure arrives as an error toast
/// without the job id). The client gives up on `/tokenize` after ~13 s (connect + request).
const COUNT_TIMEOUT_SECS: f64 = 15.0;
/// How often the override files are checked for changes by other programs (while shown).
const DISK_CHECK_SECS: f64 = 2.0;
/// The automatic count of a system prompt waits this long after the last edit.
const AUTO_COUNT_DELAY_SECS: f64 = 1.0;
/// Height of the editor footer strip.
const FOOTER_H: f32 = 24.0;

/// A `CountTokens` request waiting for its `TokenCount`.
struct TokenJob {
    id: JobId,
    item: LibraryItem,
    /// Hash of the text that was sent.
    hash: u64,
    sent_at: f64,
}

/// Size and modification time of an override file (`None`: no file).
type FileStamp = Option<(Option<SystemTime>, u64)>;

/// A finished token count and the text it belongs to.
#[derive(Clone, Copy)]
struct TokenCount {
    count: usize,
    hash: u64,
}

/// Simple numbers about a text.
#[derive(Clone, Copy, Default)]
struct TextStats {
    chars: usize,
    words: usize,
    lines: usize,
}

impl TextStats {
    fn of(text: &str) -> Self {
        Self { chars: text.chars().count(), words: text.split_whitespace().count(), lines: text.lines().count() }
    }
}

/// The selected item's text as the app reads it.
enum PreviewData {
    /// System prompts: sent as they are (characters and tokens matter).
    Prompt,
    Templates(Vec<Template>),
    Fixes(Vec<String>),
    /// (section, item) pairs.
    Checklist(Vec<(String, String)>),
    /// Guide headings: (level, text).
    Outline(Vec<(usize, String)>),
}

impl PreviewData {
    fn of(item: LibraryItem, text: &str) -> Self {
        match item {
            LibraryItem::SystemR2v | LibraryItem::SystemF2v => PreviewData::Prompt,
            LibraryItem::Templates => PreviewData::Templates(library::parse_templates(text)),
            LibraryItem::Fixes => PreviewData::Fixes(library::parse_fixes(text)),
            LibraryItem::Checklist => PreviewData::Checklist(library::parse_checklist(text)),
            LibraryItem::GuideR2v | LibraryItem::GuideF2v => PreviewData::Outline(outline(text)),
        }
    }
}

/// Parsed preview of the selected buffer, rebuilt when the text changes.
struct Preview {
    item: LibraryItem,
    hash: u64,
    stats: TextStats,
    data: PreviewData,
}

/// What the user asked for this frame (applied after drawing, when nothing is borrowed).
enum Action {
    Select(LibraryItem),
    Save(LibraryItem),
    Revert(LibraryItem),
    AskReset(LibraryItem),
    CancelReset,
    Reset(LibraryItem),
    /// The file changed on disk under unsaved edits: use the disk text, drop the edits.
    LoadDisk(LibraryItem),
    /// The file changed on disk under unsaved edits: keep them (Save then replaces the file).
    KeepMine(LibraryItem),
    Copy(LibraryItem),
    Count(LibraryItem),
    /// Show the item's override file in Explorer (or the library folder when there is none).
    OpenFolder(Option<LibraryItem>),
    CopyPath(PathBuf),
}

pub struct LibraryState {
    /// Item shown in the editor.
    pub selected: LibraryItem,
    /// Edit buffers, created from `app.library.text(item)`.
    buffers: HashMap<LibraryItem, String>,
    /// Items whose buffer differs from the saved text.
    dirty: HashSet<LibraryItem>,
    /// Token count in flight.
    token_job: Option<TokenJob>,
    /// Last token count per item.
    counts: HashMap<LibraryItem, TokenCount>,
    /// System prompt texts already counted automatically (no retry loop when a count fails).
    auto_counted: HashSet<(LibraryItem, u64)>,
    /// `app.time` of the last edit (debounces the automatic count).
    last_edit: f64,
    /// Item waiting for the "Reset to built-in" confirmation.
    confirm_reset: Option<LibraryItem>,
    preview: Option<Preview>,
    /// `config_dir()\library` (looked up once: `config_dir()` touches the disk).
    library_dir: Option<PathBuf>,
    /// The override files as last checked, in `LibraryItem::all()` order (`None`: not checked yet).
    disk_stamp: Option<Vec<FileStamp>>,
    /// `app.time` of the last check of the override files.
    disk_checked_at: f64,
    /// The last check saw a change: the next one reads the files again even if they look the same.
    disk_settle: bool,
    /// Items whose override file changed on disk under unsaved edits (Save waits for the choice).
    disk_conflicts: HashSet<LibraryItem>,
    /// The first frame ran (env overrides applied).
    started: bool,
}

impl LibraryState {
    /// Some library text has edits that are not saved yet.
    pub fn has_unsaved(&self) -> bool {
        !self.dirty.is_empty()
    }
}

impl Default for LibraryState {
    fn default() -> Self {
        Self {
            selected: LibraryItem::SystemR2v,
            buffers: HashMap::new(),
            dirty: HashSet::new(),
            token_job: None,
            counts: HashMap::new(),
            auto_counted: HashSet::new(),
            last_edit: 0.0,
            confirm_reset: None,
            preview: None,
            library_dir: None,
            disk_stamp: None,
            disk_checked_at: 0.0,
            disk_settle: false,
            disk_conflicts: HashSet::new(),
            started: false,
        }
    }
}

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    if !app.library_ws.started {
        first_open(app);
    }
    check_disk(app);
    sync_buffers(app);
    expire_token_job(app);
    update_preview(app);

    let mut actions = Vec::new();
    if app.library_ws.confirm_reset.is_none() && ctx.input_mut(|i| i.consume_shortcut(&SAVE_SHORTCUT)) {
        actions.push(Action::Save(app.library_ws.selected));
    }

    egui::SidePanel::left("lib-items")
        .resizable(true)
        .default_width(240.0)
        .width_range(200.0..=340.0)
        .frame(egui::Frame::new().fill(Palette::SIDE).inner_margin(Margin::symmetric(6, 8)))
        .show(ctx, |ui| item_list(app, ui, &mut actions));
    egui::SidePanel::right("lib-npanel")
        .resizable(true)
        .default_width(300.0)
        .width_range(260.0..=460.0)
        .frame(egui::Frame::new().fill(Palette::SIDE).inner_margin(Margin::same(8)))
        .show(ctx, |ui| n_panel(app, ui, &mut actions));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(Palette::REGION))
        .show(ctx, |ui| editor_area(app, ui, &mut actions));
    reset_dialog(app, ctx, &mut actions);

    if !actions.is_empty() {
        // Applied after drawing: show the result right away.
        ctx.request_repaint();
    }
    for action in actions {
        apply(app, ctx, action);
    }
    auto_count(app);
}

pub fn on_event(app: &mut AppState, ev: &Event) {
    if let Event::TokenCount { id, count } = ev {
        let st = &mut app.library_ws;
        if let Some(job) = st.token_job.take_if(|job| job.id == *id) {
            st.counts.insert(job.item, TokenCount { count: *count, hash: job.hash });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// State upkeep

/// Env overrides for scripted screenshots: `BH3_LIBRARY_ITEM=templates`, `BH3_DEMO=1`.
fn first_open(app: &mut AppState) {
    let st = &mut app.library_ws;
    st.started = true;
    if let Some(item) = std::env::var("BH3_LIBRARY_ITEM").ok().and_then(|v| LibraryItem::from_id(&v)) {
        st.selected = item;
    }
    if std::env::var("BH3_DEMO").as_deref() == Ok("1") {
        // An unsaved edit, so the screenshot shows the "unsaved" state.
        let anchor = "Remove the music (non_diegetic_music: N/A).\n";
        let extra = "Let her look into the camera for the last second.\n";
        let mut text = app.library.text(LibraryItem::Fixes).to_string();
        match text.find(anchor) {
            Some(pos) => text.insert_str(pos + anchor.len(), extra),
            None => text.push_str(extra),
        }
        st.buffers.insert(LibraryItem::Fixes, text);
        st.dirty.insert(LibraryItem::Fixes);
    }
}

/// Create missing buffers; buffers without edits follow the saved text.
fn sync_buffers(app: &mut AppState) {
    let st = &mut app.library_ws;
    for &item in LibraryItem::all() {
        let saved = app.library.text(item);
        match st.buffers.get_mut(&item) {
            None => {
                st.buffers.insert(item, saved.to_string());
            }
            Some(buffer) if !st.dirty.contains(&item) && buffer.as_str() != saved => *buffer = saved.to_string(),
            Some(_) => {}
        }
    }
}

/// Notice override files changed by the CLI, another window or an editor (Write jobs read them
/// fresh, so the editor must show what is used): reload `app.library` when their size or time
/// changed, at most every [`DISK_CHECK_SECS`]. Buffers without edits follow the new text; one
/// with unsaved edits is never overwritten and asks instead. Cheap; `show` calls it every frame.
pub fn check_disk(app: &mut AppState) {
    let st = &app.library_ws;
    if st.disk_stamp.is_some() && app.time - st.disk_checked_at < DISK_CHECK_SECS {
        return;
    }
    app.library_ws.disk_checked_at = app.time;
    let dir = library_dir(app);
    let stamp = disk_stamp(&dir);
    let changed = app.library_ws.disk_stamp.as_ref() != Some(&stamp);
    if !changed && !app.library_ws.disk_settle {
        return;
    }
    // Read once more next time: a file caught mid-write (or locked) has settled by then.
    app.library_ws.disk_settle = changed;
    app.library_ws.disk_stamp = Some(stamp);
    let disk = Library::load_from(&dir);
    if disk != app.library {
        merge_disk(&mut app.library_ws, &app.library, &disk);
        app.library = disk;
    }
}

/// [`FileStamp`] of every override file in `dir`.
fn disk_stamp(dir: &Path) -> Vec<FileStamp> {
    LibraryItem::all()
        .iter()
        .map(|&item| {
            let meta = std::fs::metadata(library::override_path_in(dir, item)).ok()?;
            Some((meta.modified().ok(), meta.len()))
        })
        .collect()
}

/// Items with unsaved edits whose saved text changed from `old` to `disk` get a conflict (the
/// others follow `disk` in `sync_buffers`). Edits that equal the disk text are saved already.
fn merge_disk(st: &mut LibraryState, old: &Library, disk: &Library) {
    for &item in LibraryItem::all() {
        let text = disk.text(item);
        if old.text(item) == text || !st.dirty.contains(&item) {
            continue;
        }
        if st.buffers.get(&item).is_some_and(|buffer| buffer == text) {
            st.dirty.remove(&item);
            st.disk_conflicts.remove(&item);
        } else {
            st.disk_conflicts.insert(item);
        }
    }
}

/// Forget a count that cannot get an answer any more, so the button works again: the service
/// ignores a count while Bonsai is not ready (one racing a stop fails), and a failure arrives
/// only as an error toast without the job id.
fn expire_token_job(app: &mut AppState) {
    let now = app.time;
    let ready = app.bonsai_ready();
    let st = &mut app.library_ws;
    let Some(job) = &st.token_job else { return };
    if !ready {
        // Never answered: the automatic count may try this text again once Bonsai is ready.
        st.auto_counted.remove(&(job.item, job.hash));
        st.token_job = None;
    } else if now - job.sent_at > COUNT_TIMEOUT_SECS {
        st.token_job = None;
    }
}

fn update_preview(app: &mut AppState) {
    let st = &mut app.library_ws;
    let item = st.selected;
    let Some(text) = st.buffers.get(&item) else { return };
    let hash = hash_text(text);
    if st.preview.as_ref().is_some_and(|p| p.item == item && p.hash == hash) {
        return;
    }
    st.preview = Some(Preview { item, hash, stats: TextStats::of(text), data: PreviewData::of(item, text) });
}

/// Count the shown system prompt once per text while Bonsai is ready (tokens matter for the
/// context and the prompt cache).
fn auto_count(app: &mut AppState) {
    let st = &app.library_ws;
    let item = st.selected;
    if !item.is_system_prompt()
        || !app.bonsai_ready()
        || st.token_job.is_some()
        || st.confirm_reset.is_some()
        || app.time - st.last_edit < AUTO_COUNT_DELAY_SECS
    {
        return;
    }
    let Some(text) = st.buffers.get(&item).filter(|t| !t.trim().is_empty()) else { return };
    let hash = hash_text(text);
    if st.counts.get(&item).is_some_and(|c| c.hash == hash) || st.auto_counted.contains(&(item, hash)) {
        return;
    }
    app.library_ws.auto_counted.insert((item, hash));
    request_count(app, item);
}

fn request_count(app: &mut AppState, item: LibraryItem) {
    if !app.bonsai_ready() || app.library_ws.token_job.is_some() {
        return;
    }
    let Some(text) = app.library_ws.buffers.get(&item).filter(|t| !t.trim().is_empty()).cloned() else { return };
    let hash = hash_text(&text);
    let id = app.new_job_id();
    app.send(Command::CountTokens { id, text });
    app.library_ws.token_job = Some(TokenJob { id, item, hash, sent_at: app.time });
}

// ---------------------------------------------------------------------------------------------
// Actions

fn apply(app: &mut AppState, ctx: &egui::Context, action: Action) {
    match action {
        Action::Select(item) => app.library_ws.selected = item,
        Action::Save(item) => save(app, item),
        Action::Revert(item) | Action::LoadDisk(item) => {
            let saved = app.library.text(item).to_string();
            app.library_ws.buffers.insert(item, saved);
            app.library_ws.dirty.remove(&item);
            app.library_ws.disk_conflicts.remove(&item);
        }
        Action::KeepMine(item) => {
            app.library_ws.disk_conflicts.remove(&item);
        }
        Action::AskReset(item) => app.library_ws.confirm_reset = Some(item),
        Action::CancelReset => app.library_ws.confirm_reset = None,
        Action::Reset(item) => reset(app, item),
        Action::Copy(item) => {
            if let Some(text) = app.library_ws.buffers.get(&item) {
                ctx.copy_text(text.clone());
                app.toast(ToastKind::Info, format!("Copied \"{}\" to the clipboard", item.label()));
            }
        }
        Action::Count(item) => request_count(app, item),
        Action::OpenFolder(item) => open_folder(app, item),
        Action::CopyPath(path) => {
            ctx.copy_text(path.display().to_string());
            app.toast(ToastKind::Info, "Copied the file path");
        }
    }
}

fn save(app: &mut AppState, item: LibraryItem) {
    if !app.library_ws.dirty.contains(&item) {
        return;
    }
    if app.library_ws.disk_conflicts.contains(&item) {
        // Saving now would silently replace what the other program wrote.
        let message = format!("\"{}\" changed on disk - choose Load the disk version or Keep mine first", item.label());
        app.toast(ToastKind::Warn, message);
        return;
    }
    let text = app.library_ws.buffers.get(&item).cloned().unwrap_or_default();
    let blank = text.trim().is_empty();
    match app.library.set_text(item, text) {
        Ok(()) => {
            // The library normalises the text (line endings; blank / built-in text resets it).
            let saved = app.library.text(item).to_string();
            app.library_ws.buffers.insert(item, saved);
            app.library_ws.dirty.remove(&item);
            let message = if app.library.is_overridden(item) {
                if item.is_system_prompt() {
                    format!("Saved \"{}\" - the next request uses it", item.label())
                } else {
                    format!("Saved \"{}\"", item.label())
                }
            } else if blank {
                format!("\"{}\" was blank - back to the built-in text", item.label())
            } else {
                format!("\"{}\" matches the built-in text - no override file needed", item.label())
            };
            app.toast(ToastKind::Ok, message);
        }
        Err(e) => app.toast(ToastKind::Error, format!("Could not save \"{}\": {e:#}", item.label())),
    }
}

fn reset(app: &mut AppState, item: LibraryItem) {
    app.library_ws.confirm_reset = None;
    match app.library.reset(item) {
        Ok(()) => {
            let builtin = app.library.text(item).to_string();
            app.library_ws.buffers.insert(item, builtin);
            app.library_ws.dirty.remove(&item);
            app.library_ws.disk_conflicts.remove(&item);
            app.toast(ToastKind::Ok, format!("\"{}\" is back to the built-in text", item.label()));
        }
        Err(e) => app.toast(ToastKind::Error, format!("Could not reset \"{}\": {e:#}", item.label())),
    }
}

fn open_folder(app: &mut AppState, item: Option<LibraryItem>) {
    let dir = library_dir(app);
    let file = item.map(|item| library::override_path_in(&dir, item)).filter(|p| p.is_file());
    let result = match &file {
        Some(file) => util::reveal_in_explorer(file),
        None => std::fs::create_dir_all(&dir).map_err(anyhow::Error::from).and_then(|()| util::open_path(&dir)),
    };
    if let Err(e) = result {
        app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", dir.display()));
    }
}

fn library_dir(app: &mut AppState) -> PathBuf {
    app.library_ws.library_dir.get_or_insert_with(library::library_dir).clone()
}

// ---------------------------------------------------------------------------------------------
// Left: item list

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    SystemPrompts,
    Write,
    Guides,
}

impl Group {
    fn of(item: LibraryItem) -> Self {
        match item {
            LibraryItem::SystemR2v | LibraryItem::SystemF2v => Group::SystemPrompts,
            LibraryItem::Templates | LibraryItem::Fixes | LibraryItem::Checklist => Group::Write,
            LibraryItem::GuideR2v | LibraryItem::GuideF2v => Group::Guides,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Group::SystemPrompts => "System prompts",
            Group::Write => "Write workspace",
            Group::Guides => "Guides",
        }
    }
}

fn item_icon(item: LibraryItem) -> &'static str {
    match item {
        LibraryItem::SystemR2v | LibraryItem::SystemF2v => icons::CHAT,
        LibraryItem::Templates => icons::NOTE,
        LibraryItem::Fixes => "🔧",
        LibraryItem::Checklist => icons::CHECK,
        LibraryItem::GuideR2v | LibraryItem::GuideF2v => "📖",
    }
}

fn item_list(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let edited = LibraryItem::all().iter().filter(|&&item| app.library.is_overridden(item)).count();
    let unsaved = app.library_ws.dirty.len();
    let dir = library_dir(app);

    egui::TopBottomPanel::bottom("lib-items-footer")
        .frame(egui::Frame::new().inner_margin(Margin { left: 2, right: 2, top: 6, bottom: 0 }))
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            widgets::thin_separator(ui);
            ui.add_space(4.0);
            let summary = match (edited, unsaved) {
                (0, 0) => "All texts are built-in".to_string(),
                (e, 0) => format!("{e} edited"),
                (0, u) => format!("{u} unsaved"),
                (e, u) => format!("{e} edited · {u} unsaved"),
            };
            ui.label(RichText::new(summary).small().color(Palette::TEXT_DIM));
            let open = ui
                .add_sized(
                    [ui.available_width(), 22.0],
                    egui::Button::new(format!("{} Open library folder", icons::FOLDER)),
                )
                .on_hover_text(format!("Your edited texts are saved in\n{}", dir.display()));
            if open.clicked() {
                actions.push(Action::OpenFolder(None));
            }
        });

    egui::ScrollArea::vertical().id_salt("lib-items-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let st = &app.library_ws;
        let mut group = None;
        for &item in LibraryItem::all() {
            let this = Group::of(item);
            if group != Some(this) {
                if group.is_some() {
                    ui.add_space(8.0);
                }
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    widgets::section_label(ui, this.title());
                });
                ui.add_space(2.0);
                group = Some(this);
            }
            let mut badges: Vec<(&'static str, Color32)> = Vec::new();
            if item.is_system_prompt() {
                badges.push(("sent to Bonsai", Palette::INFO));
            }
            if app.library.is_overridden(item) {
                badges.push(("edited", EDITED));
            }
            if st.dirty.contains(&item) {
                badges.push(("unsaved", Palette::WARN));
            }
            if st.disk_conflicts.contains(&item) {
                badges.push(("changed on disk", Palette::ERR));
            }
            let row = item_row(ui, item, item == st.selected, &badges).on_hover_text(item.description());
            if row.clicked() {
                actions.push(Action::Select(item));
            }
        }
    });
}

/// One list row: icon + label, then the badges (wrapping in a narrow panel) or a faint "built-in".
fn item_row(ui: &mut Ui, item: LibraryItem, selected: bool, badges: &[(&'static str, Color32)]) -> egui::Response {
    const LEFT: f32 = 28.0;
    const LINE: f32 = 18.0;
    let width = ui.available_width();
    let small = egui::FontId::proportional(11.0);

    let mut placed = Vec::new();
    let (mut x, mut line) = (LEFT, 0usize);
    for &(text, color) in badges {
        let galley = ui.painter().layout_no_wrap(text.to_owned(), small.clone(), color);
        let w = galley.size().x + 12.0;
        if x > LEFT && x + w > width - 6.0 {
            x = LEFT;
            line += 1;
        }
        placed.push((egui::vec2(x, 24.0 + line as f32 * LINE), galley, color));
        x += w + 4.0;
    }
    let height = 24.0 + (line + 1) as f32 * LINE + 2.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let painter = ui.painter_at(rect);
    let fill = if selected {
        ROW_SELECTED
    } else if response.hovered() {
        ROW_HOVER
    } else {
        Color32::TRANSPARENT
    };
    painter.rect_filled(rect, CornerRadius::same(4), fill);
    let y = rect.top() + 13.0;
    painter.text(
        egui::pos2(rect.left() + 9.0, y),
        egui::Align2::LEFT_CENTER,
        item_icon(item),
        egui::FontId::proportional(12.5),
        if selected { Palette::TEXT } else { Palette::TEXT_DIM },
    );
    painter.text(
        egui::pos2(rect.left() + LEFT, y),
        egui::Align2::LEFT_CENTER,
        item.label(),
        egui::FontId::proportional(13.5),
        if selected { Color32::WHITE } else { Palette::TEXT },
    );
    if placed.is_empty() {
        painter.text(
            egui::pos2(rect.left() + LEFT, rect.top() + 24.0 + LINE / 2.0 - 1.0),
            egui::Align2::LEFT_CENTER,
            "built-in",
            small,
            if selected { Palette::TEXT_DIM } else { Palette::TEXT_FAINT },
        );
    }
    for (offset, galley, color) in placed {
        let size = galley.size() + egui::vec2(12.0, 2.0);
        let badge = egui::Rect::from_min_size(rect.left_top() + offset, size);
        let badge_fill = if selected { Color32::from_black_alpha(110) } else { color.gamma_multiply(0.25) };
        painter.rect(
            badge,
            CornerRadius::same(8),
            badge_fill,
            Stroke::new(1.0_f32, color.gamma_multiply(0.7)),
            StrokeKind::Inside,
        );
        painter.galley(badge.min + egui::vec2(6.0, 1.0), galley, color);
    }
    response
}

// ---------------------------------------------------------------------------------------------
// Centre: editor

fn editor_area(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let item = app.library_ws.selected;
    let dirty = app.library_ws.dirty.contains(&item);
    let conflict = app.library_ws.disk_conflicts.contains(&item);
    let can_save = dirty && !conflict;
    let overridden = app.library.is_overridden(item);
    let ready = app.bonsai_ready();
    let counting = app.library_ws.token_job.as_ref().is_some_and(|job| job.item == item);
    let any_counting = app.library_ws.token_job.is_some();
    let has_text = app.library_ws.buffers.get(&item).is_some_and(|t| !t.trim().is_empty());

    widgets::area_header(ui, item.label(), |ui| {
        let compact = ui.available_width() < 560.0;
        let text = |full: String, short: &str| if compact { short.to_string() } else { full };

        // Right to left: the rightmost button comes first.
        let count_reason = if !ready {
            "Start Bonsai to count tokens (Bonsai workspace or the power switch)"
        } else if !has_text {
            "The text is empty"
        } else {
            "A count is running"
        };
        let count = ui
            .add_enabled(ready && has_text && !any_counting, egui::Button::new(text("Count tokens".into(), "Tokens")))
            .on_hover_text("Count the tokens of this text with Bonsai's tokenizer")
            .on_disabled_hover_text(count_reason);
        if count.clicked() {
            actions.push(Action::Count(item));
        }
        if counting {
            ui.spinner();
        }
        let copy = ui
            .add_enabled(has_text, egui::Button::new(text(format!("{} Copy", icons::COPY), icons::COPY)))
            .on_hover_text("Copy the whole text to the clipboard")
            .on_disabled_hover_text("The text is empty");
        if copy.clicked() {
            actions.push(Action::Copy(item));
        }
        ui.separator();
        let reset = ui
            .add_enabled(overridden, egui::Button::new(text("Reset to built-in".into(), "Reset")))
            .on_hover_text("Go back to the built-in, tested text (asks first)")
            .on_disabled_hover_text("This is the built-in text already");
        if reset.clicked() {
            actions.push(Action::AskReset(item));
        }
        let revert = ui
            .add_enabled(dirty, egui::Button::new("Revert"))
            .on_hover_text("Discard the unsaved edits (back to the saved text)")
            .on_disabled_hover_text("No unsaved edits");
        if revert.clicked() {
            actions.push(Action::Revert(item));
        }
        let mut save =
            egui::Button::new(RichText::new(text(format!("{} Save", icons::SAVE), icons::SAVE)).color(if can_save {
                Color32::WHITE
            } else {
                Palette::TEXT
            }));
        if can_save {
            save = save.fill(theme::accent(ui.ctx()));
        }
        let save = ui
            .add_enabled(can_save, save)
            .on_hover_text("Save the text (Ctrl+S) - the next request uses it")
            .on_disabled_hover_text(if conflict {
                "The file changed on disk - choose Load the disk version or Keep mine first"
            } else {
                "No unsaved edits (Ctrl+S)"
            });
        if save.clicked() {
            actions.push(Action::Save(item));
        }

        // Left of the buttons, right after the title: the file name and the state badges.
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            if !compact && ui.available_width() > 230.0 {
                ui.label(RichText::new(item.file_name()).monospace().size(11.5).color(Palette::TEXT_FAINT));
            }
            if overridden && ui.available_width() > 60.0 {
                widgets::badge(ui, "edited", EDITED)
                    .on_hover_text("Your saved version is used instead of the built-in text");
            }
            if dirty && ui.available_width() > 64.0 {
                widgets::badge(ui, "unsaved", Palette::WARN).on_hover_text("Ctrl+S saves");
            }
        });
    });
    if conflict {
        disk_conflict_strip(ui, item, actions);
    }

    // Editor above, footer strip below (drawn after the editor so it shows this frame's cursor).
    let area = ui.available_rect_before_wrap();
    let split = (area.max.y - FOOTER_H).max(area.min.y);
    let editor_rect = egui::Rect::from_min_max(area.min, egui::pos2(area.max.x, split));
    let footer_rect = egui::Rect::from_min_max(egui::pos2(area.min.x, split), area.max);
    let editor_id =
        ui.scope_builder(UiBuilder::new().max_rect(editor_rect.shrink(6.0)), |ui| editor(app, ui, item)).inner;
    ui.scope_builder(UiBuilder::new().max_rect(footer_rect), |ui| footer(app, ui, item, editor_id));
}

/// Strip under the header: the file changed on disk while the buffer has unsaved edits.
fn disk_conflict_strip(ui: &mut Ui, item: LibraryItem, actions: &mut Vec<Action>) {
    egui::Frame::new().fill(Palette::ERR.gamma_multiply(0.14)).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Inside `horizontal`, so the right-aligned row stays one row high.
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let keep =
                    ui.button("Keep mine").on_hover_text("Keep your unsaved edits - Save then replaces the file");
                if keep.clicked() {
                    actions.push(Action::KeepMine(item));
                }
                let load = ui
                    .button("Load the disk version")
                    .on_hover_text("Use the text from the file and drop your unsaved edits");
                if load.clicked() {
                    actions.push(Action::LoadDisk(item));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new(icons::WARN).color(Palette::ERR));
                    ui.add(
                        egui::Label::new(format!("{} changed on disk while you have unsaved edits", item.file_name()))
                            .truncate(),
                    )
                    .on_hover_text(
                        "Another program saved this file (Notepad, the bh3 CLI or another window of this app). \
                         Requests use the text on disk.",
                    );
                });
            });
        });
    });
}

/// The editable text, filling the area. Returns the text edit's id (for the cursor position).
fn editor(app: &mut AppState, ui: &mut Ui, item: LibraryItem) -> egui::Id {
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let rows = (((ui.available_height() - 8.0) / row_h).floor().max(3.0)) as usize;
    let st = &mut app.library_ws;
    let saved = app.library.text(item);
    let buffer = st.buffers.entry(item).or_insert_with(|| saved.to_string());
    let output = egui::ScrollArea::vertical()
        .id_salt(("lib-editor-scroll", item.id()))
        .auto_shrink([false, false])
        .show(ui, |ui| widgets::code_view(ui, &format!("lib-editor-{}", item.id()), buffer, rows, true));
    let response = output.inner;
    if response.changed() {
        let now_dirty =
            if buffer.contains('\r') { buffer.replace("\r\n", "\n") != saved } else { buffer.as_str() != saved };
        if now_dirty {
            st.dirty.insert(item);
        } else {
            st.dirty.remove(&item);
            st.disk_conflicts.remove(&item);
        }
        st.last_edit = app.time;
        ui.ctx().request_repaint();
    }
    response.id
}

fn footer(app: &AppState, ui: &mut Ui, item: LibraryItem, editor_id: egui::Id) {
    let st = &app.library_ws;
    let text = st.buffers.get(&item).map(String::as_str).unwrap_or("");
    let stats = st.preview.as_ref().filter(|p| p.item == item).map(|p| p.stats).unwrap_or_default();
    let cursor = egui::TextEdit::load_state(ui.ctx(), editor_id).and_then(|s| s.cursor.char_range());
    let tokens = token_label(app, item);

    egui::Frame::new().fill(Palette::HEADER).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_height(ui.available_height());
        ui.horizontal_centered(|ui| {
            let (state, color) = if st.disk_conflicts.contains(&item) {
                ("Changed on disk - load it or keep yours (above)", Palette::ERR)
            } else if st.dirty.contains(&item) {
                ("Unsaved changes - Ctrl+S saves", Palette::WARN)
            } else if app.library.is_overridden(item) {
                ("Your saved version", EDITED)
            } else {
                ("Built-in text", Palette::TEXT_DIM)
            };
            ui.label(RichText::new(state).small().color(color));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let dim = |s: String| RichText::new(s).small().color(Palette::TEXT_DIM);
                if let Some(tokens) = tokens {
                    ui.label(dim(tokens));
                    ui.label(dim("·".into()));
                }
                ui.label(dim(format!("{} chars", fmt_int(stats.chars))));
                ui.label(dim("·".into()));
                ui.label(dim(format!("{} lines", fmt_int(stats.lines))));
                if let Some(range) = cursor {
                    ui.label(dim("·".into()));
                    let selected = range.primary.index.abs_diff(range.secondary.index);
                    if selected > 0 {
                        ui.label(dim(format!("{} selected", fmt_int(selected))));
                    }
                    let (line, col) = line_col(text, range.primary.index);
                    ui.label(dim(format!("Ln {line}, Col {col}")));
                }
            });
        });
    });
}

/// "1,102 tokens" / "~1,102 tokens (before edits)" / "counting tokens..." for the footer.
fn token_label(app: &AppState, item: LibraryItem) -> Option<String> {
    let st = &app.library_ws;
    if st.token_job.as_ref().is_some_and(|job| job.item == item) {
        return Some("counting tokens...".into());
    }
    let count = st.counts.get(&item)?;
    let current = st.preview.as_ref().filter(|p| p.item == item).map(|p| p.hash);
    if current == Some(count.hash) {
        Some(format!("{} tokens", fmt_int(count.count)))
    } else {
        Some(format!("~{} tokens (before edits)", fmt_int(count.count)))
    }
}

// ---------------------------------------------------------------------------------------------
// Right: N-panel

fn n_panel(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let item = app.library_ws.selected;
    let file = library::override_path_in(&library_dir(app), item);
    egui::ScrollArea::vertical().id_salt("lib-npanel-scroll").auto_shrink([false, false]).show(ui, |ui| {
        widgets::panel(ui, "lib-about", "About this text", true, |ui| about_panel(app, ui, item, &file, actions));
        let title = preview_title(app, item);
        widgets::panel(ui, "lib-preview", &title, true, |ui| preview_panel(app, ui, item));
        widgets::panel(ui, "lib-tips", "Tips", true, |ui| tips_panel(ui, item));
    });
}

fn about_panel(app: &AppState, ui: &mut Ui, item: LibraryItem, file: &Path, actions: &mut Vec<Action>) {
    ui.add(egui::Label::new(item.description()).wrap());
    if item.is_system_prompt() {
        ui.add_space(4.0);
        callout(
            ui,
            Palette::WARN,
            icons::WARN,
            "Changing it changes what Bonsai writes. The tested version is built in - Reset to built-in \
             restores it. Keep it identical between requests so llama-server can reuse its prompt cache.",
        );
    }
    ui.add_space(6.0);
    widgets::prop_row(ui, "Used by", |ui| ui.add(egui::Label::new(used_by(item)).wrap()));
    widgets::prop_row(ui, "In use", |ui| {
        if app.library.is_overridden(item) {
            ui.label(RichText::new("Your saved version").color(EDITED))
        } else {
            ui.label("Built-in text")
        }
    });
    if app.library_ws.dirty.contains(&item) {
        widgets::prop_row(ui, "", |ui| ui.label(RichText::new("+ unsaved edits").color(Palette::WARN)));
    }
    ui.add_space(4.0);
    widgets::section_label(ui, "Override file");
    let path = file.display().to_string();
    ui.add(egui::Label::new(RichText::new(&path).monospace().size(11.5).color(Palette::TEXT_DIM)).wrap())
        .on_hover_text(if app.library.is_overridden(item) {
            "Your saved version (deleted by Reset to built-in)"
        } else {
            "Created when you save an edit"
        });
    ui.horizontal(|ui| {
        let open =
            ui.button(format!("{} Open folder", icons::FOLDER)).on_hover_text(if app.library.is_overridden(item) {
                "Show the file in Explorer"
            } else {
                "Open the library folder in Explorer"
            });
        if open.clicked() {
            actions.push(Action::OpenFolder(Some(item)));
        }
        if widgets::icon_button(ui, icons::COPY, "Copy the file path").clicked() {
            actions.push(Action::CopyPath(file.to_path_buf()));
        }
    });
}

fn used_by(item: LibraryItem) -> &'static str {
    match item {
        LibraryItem::SystemR2v => "Every r2v request in Write; Chat (H3 r2v)",
        LibraryItem::SystemF2v => "Every f2v request in Write; Chat (H3 f2v)",
        LibraryItem::Templates => "Write > Templates",
        LibraryItem::Fixes => "Write > Fix (follow-up buttons)",
        LibraryItem::Checklist => "Write > Checklist",
        LibraryItem::GuideR2v | LibraryItem::GuideF2v => "Reading only - not sent to Bonsai",
    }
}

fn preview_title(app: &AppState, item: LibraryItem) -> String {
    let Some(preview) = app.library_ws.preview.as_ref().filter(|p| p.item == item) else {
        return "Preview".into();
    };
    let plural = |n: usize, one: &str, many: &str| format!("Preview · {n} {}", if n == 1 { one } else { many });
    match &preview.data {
        PreviewData::Prompt => "Preview".into(),
        PreviewData::Templates(t) => plural(t.len(), "template", "templates"),
        PreviewData::Fixes(f) => plural(f.len(), "fix", "fixes"),
        PreviewData::Checklist(c) => plural(c.len(), "item", "items"),
        PreviewData::Outline(o) => plural(o.len(), "heading", "headings"),
    }
}

fn preview_panel(app: &AppState, ui: &mut Ui, item: LibraryItem) {
    let Some(preview) = app.library_ws.preview.as_ref().filter(|p| p.item == item) else {
        ui.label(RichText::new("Nothing to show").color(Palette::TEXT_DIM));
        return;
    };
    match &preview.data {
        PreviewData::Prompt => prompt_preview(app, ui, item, preview),
        PreviewData::Templates(templates) => templates_preview(ui, templates),
        PreviewData::Fixes(fixes) => fixes_preview(ui, fixes),
        PreviewData::Checklist(items) => checklist_preview(ui, items),
        PreviewData::Outline(headings) => outline_preview(ui, headings, preview.stats),
    }
}

fn prompt_preview(app: &AppState, ui: &mut Ui, item: LibraryItem, preview: &Preview) {
    let stats = preview.stats;
    widgets::prop_row(ui, "Characters", |ui| ui.label(fmt_int(stats.chars)));
    widgets::prop_row(ui, "Words", |ui| ui.label(fmt_int(stats.words)));
    widgets::prop_row(ui, "Lines", |ui| ui.label(fmt_int(stats.lines)));
    let st = &app.library_ws;
    let count = st.counts.get(&item).copied();
    let fresh = count.filter(|c| c.hash == preview.hash);
    widgets::prop_row(ui, "Tokens", |ui| {
        if st.token_job.as_ref().is_some_and(|job| job.item == item) {
            ui.spinner();
            ui.label(RichText::new("counting...").color(Palette::TEXT_DIM));
        } else if let Some(c) = fresh {
            ui.label(RichText::new(fmt_int(c.count)).strong());
        } else if let Some(c) = count {
            ui.label(format!("~{}", fmt_int(c.count)));
            ui.label(RichText::new("before edits").small().color(Palette::TEXT_DIM));
        } else if app.bonsai_ready() {
            ui.label(RichText::new("not counted").color(Palette::TEXT_DIM));
        } else {
            ui.label(RichText::new("start Bonsai to count").color(Palette::TEXT_DIM));
        }
    });
    let n_ctx = app.status.props.as_ref().map(|p| p.n_ctx).filter(|&n| n > 0);
    if let (Some(c), Some(n_ctx)) = (count, n_ctx) {
        let pct = c.count as f64 * 100.0 / n_ctx as f64;
        widgets::prop_row(ui, "Context", |ui| {
            ui.label(format!("{pct:.1} %")).on_hover_text(format!(
                "of the {}-token context; the rest is for the request and the answer",
                fmt_int(n_ctx as usize)
            ));
            ui.label(RichText::new(format!("of {}", fmt_int(n_ctx as usize))).small().color(Palette::TEXT_DIM));
        });
    }
    ui.add_space(4.0);
    ui.add(
        egui::Label::new(
            RichText::new("Sent as it is before every request; llama-server caches it while it stays the same.")
                .small()
                .color(Palette::TEXT_DIM),
        )
        .wrap(),
    );
}

fn templates_preview(ui: &mut Ui, templates: &[Template]) {
    if templates.is_empty() {
        empty_note(ui, "No templates found - check the \"==== r2v ====\" and \"--- title ---\" lines.");
        return;
    }
    let r2v = templates.iter().filter(|t| t.mode == Mode::R2v).count();
    ui.label(
        RichText::new(format!("{r2v} r2v · {} f2v - hover one to see its text", templates.len() - r2v))
            .small()
            .color(Palette::TEXT_DIM),
    );
    ui.add_space(2.0);
    for template in templates {
        let row = ui.horizontal(|ui| {
            let (label, color) = match template.mode {
                Mode::R2v => ("r2v", R2V_COLOR),
                Mode::F2v => ("f2v", F2V_COLOR),
            };
            widgets::badge(ui, label, color);
            ui.add(egui::Label::new(&template.title).wrap());
        });
        row.response.on_hover_ui(|ui| {
            ui.set_max_width(440.0);
            ui.label(RichText::new(&template.text).monospace().size(11.5));
        });
    }
}

fn fixes_preview(ui: &mut Ui, fixes: &[String]) {
    if fixes.is_empty() {
        empty_note(ui, "No fixes found - write one fix per line.");
        return;
    }
    for (i, fix) in fixes.iter().enumerate() {
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(20.0, 18.0), Layout::right_to_left(Align::Min), |ui| {
                ui.label(RichText::new(format!("{}.", i + 1)).color(Palette::TEXT_DIM));
            });
            ui.add(egui::Label::new(fix).wrap());
        });
    }
}

fn checklist_preview(ui: &mut Ui, items: &[(String, String)]) {
    if items.is_empty() {
        empty_note(ui, "No items found - checklist items start with \"[ ]\".");
        return;
    }
    let mut section: Option<&str> = None;
    for (name, text) in items {
        if section != Some(name.as_str()) {
            if section.is_some() {
                ui.add_space(4.0);
            }
            let heading = if name.is_empty() { "Checklist" } else { name.as_str() };
            ui.add(egui::Label::new(RichText::new(heading).small().strong().color(Palette::TEXT_DIM)).wrap());
            section = Some(name.as_str());
        }
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 18.0), Sense::hover());
            let square = egui::Rect::from_center_size(rect.center(), egui::vec2(9.0, 9.0));
            ui.painter().rect_stroke(
                square,
                CornerRadius::same(2),
                Stroke::new(1.0_f32, Palette::TEXT_DIM),
                StrokeKind::Inside,
            );
            ui.add(egui::Label::new(text).wrap());
        });
    }
}

fn outline_preview(ui: &mut Ui, headings: &[(usize, String)], stats: TextStats) {
    ui.label(
        RichText::new(format!("{} words · {} lines", fmt_int(stats.words), fmt_int(stats.lines)))
            .small()
            .color(Palette::TEXT_DIM),
    );
    if headings.is_empty() {
        empty_note(ui, "No headings (lines starting with #).");
        return;
    }
    for (level, text) in headings {
        ui.horizontal_top(|ui| {
            ui.add_space((level.saturating_sub(1) * 12) as f32);
            let text = if *level == 1 { RichText::new(text).strong() } else { RichText::new(text) };
            ui.add(egui::Label::new(text).wrap());
        });
    }
}

fn tips_panel(ui: &mut Ui, item: LibraryItem) {
    let specific: &[&str] = match item {
        LibraryItem::SystemR2v | LibraryItem::SystemF2v => &[
            "Change one thing at a time, then test it with a request in Write: the Check panel shows \
             whether the prompt format still holds.",
            "The first request after a change is slower: llama-server reads the new system prompt once, \
             later requests reuse its cache.",
        ],
        LibraryItem::Templates => &[
            "\"==== r2v ... ====\" / \"==== f2v ... ====\" starts a section, \"--- title ---\" a template.",
            "Put what the user fills in between < >, e.g. <age, hair, clothes>.",
            "Sections without r2v / f2v in the heading are ignored.",
        ],
        LibraryItem::Fixes => &[
            "One fix per line. The first paragraph (title in capitals) and a closing note are not fixes.",
            "Lines starting with # or // are comments.",
        ],
        LibraryItem::Checklist => &[
            "\"[ ]\" starts an item; indented lines continue it.",
            "Any other line names the section of the items below it.",
        ],
        LibraryItem::GuideR2v | LibraryItem::GuideF2v => &[
            "Markdown: lines starting with # are headings (see the outline above).",
            "Guides are for you to read - Bonsai never sees them.",
        ],
    };
    let general = [
        "Ctrl+S saves. The next request uses the saved text - no restart needed.",
        "Unsaved edits are kept while you switch items, but not when the app closes.",
        "A file saved by another program (Notepad, the bh3 CLI) shows up here within seconds.",
    ];
    for tip in specific.iter().chain(general.iter()) {
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("•").color(Palette::TEXT_DIM));
            ui.add(egui::Label::new(RichText::new(*tip).color(Palette::TEXT_DIM)).wrap());
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Reset confirmation

fn reset_dialog(app: &mut AppState, ctx: &egui::Context, actions: &mut Vec<Action>) {
    let Some(item) = app.library_ws.confirm_reset else { return };
    let file = library::override_path_in(&library_dir(app), item);
    let dirty = app.library_ws.dirty.contains(&item);
    let modal = egui::Modal::new(egui::Id::new("lib-reset-confirm")).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.label(RichText::new(format!("Reset \"{}\" to the built-in text?", item.label())).strong().size(15.0));
        ui.add_space(6.0);
        ui.add(egui::Label::new("The built-in, tested text is used again and your saved version is deleted:").wrap());
        ui.add(
            egui::Label::new(RichText::new(file.display().to_string()).monospace().size(11.5).color(Palette::TEXT_DIM))
                .wrap(),
        );
        if dirty {
            ui.add_space(2.0);
            ui.label(RichText::new("Your unsaved edits are discarded too.").color(Palette::WARN));
        }
        ui.add_space(10.0);
        let mut choice = None;
        // Inside `horizontal`, so the right-aligned row stays one row high.
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").on_hover_text("Keep your version (Esc)").clicked() {
                    choice = Some(false);
                }
                let reset = egui::Button::new(RichText::new("Reset to built-in").color(Color32::WHITE))
                    .fill(Color32::from_rgb(168, 58, 48));
                if ui.add(reset).clicked() {
                    choice = Some(true);
                }
            });
        });
        choice
    });
    match modal.inner {
        Some(true) => actions.push(Action::Reset(item)),
        Some(false) => actions.push(Action::CancelReset),
        None if modal.should_close() => actions.push(Action::CancelReset),
        None => {}
    }
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

fn empty_note(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(Palette::WARN)).wrap());
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Markdown headings (`#` to `####`) as (level, text), outside code blocks.
fn outline(text: &str) -> Vec<(usize, String)> {
    let mut in_code = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        let level = trimmed.chars().take_while(|&c| c == '#').count();
        if in_code || !(1..=4).contains(&level) {
            continue;
        }
        let rest = &trimmed[level..];
        if rest.starts_with(' ') && !rest.trim().is_empty() {
            out.push((level, rest.trim().to_string()));
        }
    }
    out
}

/// 1-based line and column of a character offset.
fn line_col(text: &str, char_index: usize) -> (usize, usize) {
    let (mut line, mut col) = (1, 1);
    for c in text.chars().take(char_index) {
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// `12345` -> `12,345`.
fn fmt_int(n: usize) -> String {
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
    fn thousands_and_positions() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1102), "1,102");
        assert_eq!(fmt_int(1_234_567), "1,234,567");
        assert_eq!(line_col("ab\ncd", 0), (1, 1));
        assert_eq!(line_col("ab\ncd", 4), (2, 2));
    }

    /// A fresh, empty folder for one test (unique per process and test).
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bh3-library-ws-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn disk_changes_follow_or_ask() {
        let dir = temp_dir("merge");
        let old = Library::builtin();
        let mut disk = Library::load_from(&dir);
        for item in [LibraryItem::SystemR2v, LibraryItem::Fixes, LibraryItem::Templates] {
            disk.set_text_in(&dir, item, format!("Saved in Notepad: {}\n", item.id())).unwrap();
        }
        let mut st = LibraryState::default();
        // Fixes: unsaved edits on top of a file that changed -> asks.
        st.buffers.insert(LibraryItem::Fixes, "my edit\n".into());
        st.dirty.insert(LibraryItem::Fixes);
        // Templates: the edits equal what is on disk now -> saved already.
        st.buffers.insert(LibraryItem::Templates, disk.text(LibraryItem::Templates).into());
        st.dirty.insert(LibraryItem::Templates);
        // Checklist: unsaved edits, but its file did not change -> nothing to ask.
        st.buffers.insert(LibraryItem::Checklist, "my list\n".into());
        st.dirty.insert(LibraryItem::Checklist);
        merge_disk(&mut st, &old, &disk);
        assert_eq!(st.disk_conflicts, HashSet::from([LibraryItem::Fixes]));
        assert_eq!(st.dirty, HashSet::from([LibraryItem::Fixes, LibraryItem::Checklist]));
        // The unsaved buffer is kept (never overwritten by the reload).
        assert_eq!(st.buffers[&LibraryItem::Fixes], "my edit\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_stamp_sees_saves_and_deletes() {
        let dir = temp_dir("stamp");
        let empty = disk_stamp(&dir);
        assert_eq!(empty.len(), LibraryItem::all().len());
        assert!(empty.iter().all(Option::is_none));
        let mut lib = Library::load_from(&dir);
        lib.set_text_in(&dir, LibraryItem::Checklist, "[ ] one\n".into()).unwrap();
        let saved = disk_stamp(&dir);
        assert_ne!(saved, empty);
        lib.set_text_in(&dir, LibraryItem::Checklist, "[ ] one\n[ ] two\n".into()).unwrap();
        assert_ne!(disk_stamp(&dir), saved, "a longer file is a change");
        lib.reset_in(&dir, LibraryItem::Checklist).unwrap();
        assert_eq!(disk_stamp(&dir), empty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outline_skips_code_blocks() {
        let text = "# Title\nintro\n## 1. Part\n```\n# not a heading\n```\n#tag\n### Deep";
        assert_eq!(outline(text), vec![(1, "Title".into()), (2, "1. Part".into()), (3, "Deep".into())]);
    }
}
