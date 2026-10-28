//! History workspace: every written prompt.
//!
//! Layout (Blender style): filter region on the left; in the centre a resizable table of the
//! prompts above the preview (request + prompt side by side, metadata, actions).
//!
//! The list (`bh3::history::list()`) is read when the workspace is first shown, again after a
//! prompt was saved (`WriteDone`, re-read on the next show) and with Refresh (F5) - never per
//! frame. Keys (when no text field has focus): Up/Down select, Enter opens in Write, Delete asks
//! to delete, Ctrl+C copies the prompt, S stars; Ctrl+F searches.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use bh3::gen::Mode;
use bh3::history::{self, HistoryEntry};
use bh3::service::Event;
use eframe::egui::{
    self, Align, Color32, CornerRadius, Id, Key, Label, Layout, Margin, Modifiers, RichText, Sense, Stroke, TextStyle,
    Ui,
};
use egui_extras::{Column, TableBuilder};

use super::app::{AppState, ToastKind, Workspace};
use super::chat::{came_back, confirm_popup};
use super::theme::Palette;
use super::util;
use super::widgets::{self, icons};

const SEARCH_ID: &str = "bh3-history-search";
/// Star colour (Blender's keyframe yellow).
const STAR: Color32 = Color32::from_rgb(240, 196, 64);
const ROW_H: f32 = 22.0;
/// Below this width the request and the prompt are stacked instead of side by side.
const SIDE_BY_SIDE_MIN: f32 = 720.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ModeFilter {
    #[default]
    All,
    R2v,
    F2v,
}

impl ModeFilter {
    fn accepts(self, mode: Mode) -> bool {
        match self {
            ModeFilter::All => true,
            ModeFilter::R2v => mode == Mode::R2v,
            ModeFilter::F2v => mode == Mode::F2v,
        }
    }
}

/// Table column the list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SortBy {
    Star,
    #[default]
    Date,
    Mode,
    Title,
    Score,
    Speed,
}

impl SortBy {
    /// Direction of the first click: A-Z for text, highest / newest first for the rest.
    fn starts_ascending(self) -> bool {
        matches!(self, SortBy::Mode | SortBy::Title)
    }
}

/// What the filtered, sorted view was built from.
#[derive(Debug, Clone, PartialEq)]
struct ViewKey {
    query: String,
    mode: ModeFilter,
    starred_only: bool,
    sort: SortBy,
    ascending: bool,
    generation: u64,
}

#[derive(Default)]
pub struct HistoryState {
    loaded: bool,
    /// A prompt was saved since the last read: read again on the next show.
    stale: bool,
    /// Newest first (as `history::list` returns them).
    entries: Vec<HistoryEntry>,
    /// Bumped whenever `entries` changes (invalidates the view).
    generation: u64,
    selected: Option<String>,
    /// Select this id after the next read (the prompt just written).
    select_after_reload: Option<String>,
    query: String,
    mode: ModeFilter,
    starred_only: bool,
    sort: SortBy,
    /// `false` = the column's natural order (newest / best first; A-Z for text).
    ascending: bool,
    /// Indices into `entries` that pass the filter, in display order.
    view: Vec<usize>,
    view_key: Option<ViewKey>,
    confirm_delete: Option<String>,
    scroll_to_selected: bool,
    focus_search: bool,
    dir: Option<PathBuf>,
    /// Size of the history folder at the last read.
    total_bytes: u64,
    /// egui pass in which the workspace was last shown (see [`came_back`]).
    shown_pass: Option<u64>,
}

impl HistoryState {
    fn entry(&self, id: &str) -> Option<&HistoryEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn selected_entry(&self) -> Option<&HistoryEntry> {
        self.selected.as_deref().and_then(|id| self.entry(id))
    }

    /// Position of the selection in the view.
    fn selected_row(&self) -> Option<usize> {
        let id = self.selected.as_deref()?;
        self.view.iter().position(|&i| self.entries.get(i).is_some_and(|e| e.id == id))
    }

    fn row_id(&self, row: usize) -> Option<String> {
        self.view.get(row).and_then(|&i| self.entries.get(i)).map(|e| e.id.clone())
    }

    fn filter_active(&self) -> bool {
        !self.query.trim().is_empty() || self.mode != ModeFilter::All || self.starred_only
    }

    fn clear_filter(&mut self) {
        self.query.clear();
        self.mode = ModeFilter::All;
        self.starred_only = false;
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

pub fn show(app: &mut AppState, ctx: &egui::Context) {
    if came_back(ctx, &mut app.history.shown_pass) {
        // Another workspace was shown (Ctrl+1..6 also work under the popup): the question must not
        // come back and take the next Enter.
        app.history.confirm_delete = None;
    }
    if !app.history.loaded || app.history.stale {
        reload(app);
    }
    rebuild_view(&mut app.history);
    shortcuts(app, ctx);
    rebuild_view(&mut app.history);

    egui::SidePanel::left("history-filter")
        .resizable(true)
        .default_width(230.0)
        .width_range(190.0..=380.0)
        .frame(egui::Frame::new().fill(Palette::SIDE).inner_margin(Margin::symmetric(8, 8)))
        .show(ctx, |ui| filter_panel(app, ui));
    egui::CentralPanel::default().frame(egui::Frame::new().fill(Palette::REGION)).show(ctx, |ui| {
        // The preview keeps about 300 px (when the window allows); the list starts at ~40 %.
        let available = ui.available_height();
        let list_max = (available - 300.0).max(130.0);
        egui::TopBottomPanel::top("history-list")
            .resizable(true)
            .default_height((available * 0.4).clamp(130.0, list_max))
            .height_range(110.0..=list_max)
            .frame(egui::Frame::new().fill(Palette::REGION))
            .show_inside(ui, |ui| list_area(app, ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Palette::REGION))
            .show_inside(ui, |ui| preview_area(app, ui));
    });
    confirm_dialog(app, ctx);
}

pub fn on_event(app: &mut AppState, ev: &Event) {
    if let Event::WriteDone { outcome, .. } = ev {
        if let Some(id) = &outcome.history_id {
            app.history.stale = true;
            app.history.select_after_reload = Some(id.clone());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------------

fn demo_mode() -> bool {
    std::env::var("BH3_DEMO").is_ok_and(|v| v.trim() == "1")
}

/// Read the history folder (keeps the selection when it still exists).
fn reload(app: &mut AppState) {
    let first_load = !app.history.loaded;
    let dir = history::history_dir();
    let st = &mut app.history;
    st.entries = history::list();
    st.total_bytes = folder_size(&dir);
    st.dir = Some(dir);
    st.loaded = true;
    st.stale = false;
    st.generation += 1;
    if let Some(id) = st.select_after_reload.take() {
        if st.entry(&id).is_some() {
            st.selected = Some(id);
            st.scroll_to_selected = true;
        }
    }
    if st.selected_entry().is_none() {
        // Screenshots (`BH3_DEMO=1`) open on a follow-up fix, which shows every part of the preview.
        let demo_pick =
            (first_load && demo_mode()).then(|| st.entries.iter().find(|e| e.parent_id.is_some())).flatten();
        st.selected = demo_pick.or(st.entries.first()).map(|e| e.id.clone());
        st.scroll_to_selected = true;
    }
}

fn folder_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|read| read.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

/// Rebuild the filtered, sorted view when the filter, the sort or the entries changed.
fn rebuild_view(st: &mut HistoryState) {
    let key = ViewKey {
        query: st.query.clone(),
        mode: st.mode,
        starred_only: st.starred_only,
        sort: st.sort,
        ascending: st.ascending,
        generation: st.generation,
    };
    if st.view_key.as_ref() == Some(&key) {
        return;
    }
    let entries = &st.entries;
    let mut view: Vec<usize> = (0..entries.len())
        .filter(|&i| {
            let e = &entries[i];
            key.mode.accepts(e.mode) && (!key.starred_only || e.starred) && e.matches(&key.query)
        })
        .collect();
    // `entries` is newest first, so the index is the date order; it also breaks ties.
    let natural = |a: &usize, b: &usize| -> Ordering {
        let (x, y) = (&entries[*a], &entries[*b]);
        let by = match key.sort {
            SortBy::Date => Ordering::Equal,
            SortBy::Star => y.starred.cmp(&x.starred),
            SortBy::Mode => x.mode.as_str().cmp(y.mode.as_str()),
            SortBy::Title => x.title.to_lowercase().cmp(&y.title.to_lowercase()),
            SortBy::Score => y.lint_score.cmp(&x.lint_score),
            SortBy::Speed => speed(y).total_cmp(&speed(x)),
        };
        by.then(a.cmp(b))
    };
    let reverse = key.ascending != key.sort.starts_ascending();
    view.sort_by(|a, b| if reverse { natural(b, a) } else { natural(a, b) });
    st.view = view;
    st.view_key = Some(key);
}

/// Generation speed of an entry (0 without timings).
fn speed(entry: &HistoryEntry) -> f64 {
    entry.timings.as_ref().map_or(0.0, |t| t.predicted_per_second)
}

// ---------------------------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------------------------

enum Action {
    Select(String),
    Open(String),
    ToggleStar(String),
    CopyPrompt(String),
    CopyRequest(String),
    Export(String),
    AskDelete(String),
    SelectParent(String),
    Sort(SortBy),
    Refresh,
    ClearFilter,
    GoWrite,
    OpenFolder,
}

fn apply(app: &mut AppState, ctx: &egui::Context, action: Action) {
    match action {
        Action::Select(id) => app.history.selected = Some(id),
        Action::Open(id) => open_in_write(app, &id),
        Action::ToggleStar(id) => toggle_star(app, &id),
        Action::CopyPrompt(id) => {
            if let Some(entry) = app.history.entry(&id) {
                ctx.copy_text(entry.output.clone());
                app.toast(ToastKind::Ok, "Prompt copied.");
            }
        }
        Action::CopyRequest(id) => {
            if let Some(entry) = app.history.entry(&id) {
                ctx.copy_text(entry.original_request().to_string());
                app.toast(ToastKind::Ok, "Request copied.");
            }
        }
        Action::Export(id) => export(app, &id),
        Action::AskDelete(id) => app.history.confirm_delete = Some(id),
        Action::SelectParent(id) => select_parent(app, &id),
        Action::Sort(by) => {
            let st = &mut app.history;
            if st.sort == by {
                st.ascending = !st.ascending;
            } else {
                st.sort = by;
                st.ascending = by.starts_ascending();
            }
        }
        Action::Refresh => {
            reload(app);
            let n = app.history.entries.len();
            app.toast(ToastKind::Info, format!("History reloaded: {n} prompt{}.", if n == 1 { "" } else { "s" }));
        }
        Action::ClearFilter => app.history.clear_filter(),
        Action::GoWrite => app.workspace = Workspace::Write,
        Action::OpenFolder => {
            let dir = app.history.dir.clone().unwrap_or_else(history::history_dir);
            if let Err(e) = util::open_path(&dir) {
                app.toast(ToastKind::Error, format!("Could not open {}: {e:#}", dir.display()));
            }
        }
    }
}

fn open_in_write(app: &mut AppState, id: &str) {
    let Some(entry) = app.history.entry(id).cloned() else {
        return;
    };
    app.write.load_entry(&entry);
    app.workspace = Workspace::Write;
}

fn toggle_star(app: &mut AppState, id: &str) {
    let Some(starred) = app.history.entry(id).map(|e| e.starred) else {
        return;
    };
    match history::set_starred(id, !starred) {
        Ok(updated) => {
            if let Some(entry) = app.history.entries.iter_mut().find(|e| e.id == id) {
                *entry = updated;
            }
            app.history.generation += 1;
        }
        Err(e) => app.toast(ToastKind::Error, format!("Could not save the star: {e:#}")),
    }
}

fn export(app: &mut AppState, id: &str) {
    let Some(entry) = app.history.entry(id).cloned() else {
        return;
    };
    let name = format!("{}-{}.txt", entry.mode.as_str(), util::slug(&entry.title, 48));
    let Some(path) = util::pick_save_txt("Export prompt", &name) else {
        return;
    };
    match history::export_txt(&entry, &path) {
        Ok(()) => app.toast(ToastKind::Ok, format!("Saved {}", path.display())),
        Err(e) => app.toast(ToastKind::Error, format!("Could not export the prompt: {e:#}")),
    }
}

/// Delete the entry's file; `Ok(false)` when it was already gone (`bh3 history delete`, Explorer),
/// so its row can still be removed from the list.
fn delete_file(dir: &Path, id: &str) -> anyhow::Result<bool> {
    match history::delete_in(dir, id) {
        Ok(()) => Ok(true),
        Err(_) if !history::entry_path_in(dir, id).exists() => Ok(false),
        Err(e) => Err(e),
    }
}

fn delete_entry(app: &mut AppState, id: &str) {
    let removed = match delete_file(&history::history_dir(), id) {
        Ok(removed) => removed,
        Err(e) => {
            app.toast(ToastKind::Error, format!("Could not delete the prompt: {e:#}"));
            return;
        }
    };
    let st = &mut app.history;
    let row = st.selected_row();
    st.entries.retain(|e| e.id != id);
    st.generation += 1;
    rebuild_view(st);
    if st.selected.as_deref() == Some(id) {
        // Select the neighbour that moved into its row (or the new last row).
        let next = row.and_then(|r| st.row_id(r).or_else(|| st.row_id(st.view.len().saturating_sub(1))));
        st.selected = next;
        st.scroll_to_selected = true;
    }
    st.total_bytes = st.dir.as_deref().map(folder_size).unwrap_or(0);
    let text = if removed { "Prompt deleted." } else { "The prompt was already deleted: removed from the list." };
    app.toast(ToastKind::Info, text);
}

/// Follow the `parent_id` link of a follow-up fix.
fn select_parent(app: &mut AppState, parent: &str) {
    if app.history.entry(parent).is_none() {
        app.toast(ToastKind::Warn, "The prompt this one fixed is no longer in the history.");
        return;
    }
    let st = &mut app.history;
    st.selected = Some(parent.to_string());
    if st.selected_row().is_none() {
        // Hidden by the filter: show everything so the selection is visible.
        st.clear_filter();
    }
    st.scroll_to_selected = true;
}

fn move_selection(st: &mut HistoryState, delta: isize) {
    if st.view.is_empty() {
        return;
    }
    let last = st.view.len() - 1;
    let next = match st.selected_row() {
        Some(row) => row.saturating_add_signed(delta).min(last),
        None if delta > 0 => 0,
        None => last,
    };
    st.selected = st.row_id(next);
    st.scroll_to_selected = true;
}

fn shortcuts(app: &mut AppState, ctx: &egui::Context) {
    if app.history.confirm_delete.is_some() || app.show_setup {
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
        app.history.focus_search = true;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F5)) {
        apply(app, ctx, Action::Refresh);
    }
    if ctx.wants_keyboard_input() {
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
        move_selection(&mut app.history, 1);
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
        move_selection(&mut app.history, -1);
    }
    let Some(id) = app.history.selected_entry().map(|e| e.id.clone()) else {
        return;
    };
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
        apply(app, ctx, Action::Open(id));
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete)) {
        apply(app, ctx, Action::AskDelete(id));
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::S)) {
        apply(app, ctx, Action::ToggleStar(id.clone()));
    }
    // Ctrl+C arrives as a Copy event; selected label text keeps its own copy.
    let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
    if copy && !egui::text_selection::LabelSelectionState::load(ctx).has_selection() {
        apply(app, ctx, Action::CopyPrompt(id));
    }
}

// ---------------------------------------------------------------------------------------------
// Left: filter
// ---------------------------------------------------------------------------------------------

fn filter_panel(app: &mut AppState, ui: &mut Ui) {
    let mut actions = Vec::new();
    egui::ScrollArea::vertical().id_salt("history-filter-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let st = &mut app.history;
        let total = st.entries.len();
        let shown = st.view.len();
        widgets::panel(ui, "history-filter", "Filter", true, |ui| {
            let search = egui::TextEdit::singleline(&mut st.query)
                .id(Id::new(SEARCH_ID))
                .hint_text(format!("{} Search (Ctrl+F)", icons::SEARCH))
                .desired_width(f32::INFINITY);
            let resp = ui.add(search).on_hover_text("Words in the title, request, prompt, mode or id");
            if st.focus_search {
                resp.request_focus();
                st.focus_search = false;
            }
            ui.add_space(4.0);
            widgets::prop_row(ui, "Mode", |ui| {
                widgets::enum_buttons(
                    ui,
                    &mut st.mode,
                    &[(ModeFilter::All, "All"), (ModeFilter::R2v, "r2v"), (ModeFilter::F2v, "f2v")],
                );
            });
            widgets::prop_row(ui, "", |ui| {
                ui.checkbox(&mut st.starred_only, format!("{} Starred only", icons::STAR))
                    .on_hover_text("Show only starred prompts (S stars the selection)");
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let color = if shown < total { Palette::WARN } else { Palette::TEXT_DIM };
                ui.label(RichText::new(format!("{shown} of {total} shown")).small().color(color));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let clear = ui
                        .add_enabled(st.filter_active(), egui::Button::new("Clear").small())
                        .on_hover_text("Show every prompt")
                        .on_disabled_hover_text("No filter is set");
                    if clear.clicked() {
                        actions.push(Action::ClearFilter);
                    }
                });
            });
        });
        widgets::panel(ui, "history-folder", "Folder", true, |ui| {
            let dir = st.dir.clone().unwrap_or_default();
            ui.add(Label::new(RichText::new(dir.display().to_string()).small().color(Palette::TEXT_DIM)).wrap());
            ui.label(
                RichText::new(format!(
                    "{total} prompt{}  ·  {}",
                    if total == 1 { "" } else { "s" },
                    fmt_bytes(st.total_bytes)
                ))
                .small()
                .color(Palette::TEXT_FAINT),
            );
            ui.add_space(4.0);
            let open = ui
                .add(
                    egui::Button::new(format!("{} Open folder", icons::FOLDER))
                        .min_size(egui::vec2(ui.available_width(), 22.0)),
                )
                .on_hover_text("Open the history folder in Explorer");
            if open.clicked() {
                actions.push(Action::OpenFolder);
            }
        });
        widgets::panel(ui, "history-keys", "Keys", false, |ui| {
            egui::Grid::new("history-keys-grid").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
                for (key, what) in [
                    ("Double-click, Enter", "Open in Write"),
                    ("Up, Down", "Select"),
                    ("Ctrl+C", "Copy the prompt"),
                    ("S", "Star"),
                    ("Delete", "Delete"),
                    ("Ctrl+F", "Search"),
                    ("F5", "Reload"),
                ] {
                    ui.label(RichText::new(key).small().color(Palette::TEXT));
                    ui.label(RichText::new(what).small().color(Palette::TEXT_DIM));
                    ui.end_row();
                }
            });
        });
    });
    for action in actions {
        apply(app, ui.ctx(), action);
    }
}

// ---------------------------------------------------------------------------------------------
// Centre top: the table
// ---------------------------------------------------------------------------------------------

fn list_area(app: &mut AppState, ui: &mut Ui) {
    let mut actions = Vec::new();
    let shown = app.history.view.len();
    let total = app.history.entries.len();
    widgets::area_header(ui, "Prompts", |ui| {
        if widgets::icon_button(ui, icons::RESTART, "Reload the history folder (F5)").clicked() {
            actions.push(Action::Refresh);
        }
        let text = if shown == total { format!("{total}") } else { format!("{shown} of {total}") };
        ui.label(RichText::new(text).small().color(Palette::TEXT_DIM));
    });
    if total == 0 {
        empty_list(
            ui,
            "No prompts yet",
            "Prompts written in the Write workspace are saved here.",
            "Go to Write",
            Action::GoWrite,
            &mut actions,
        );
    } else if shown == 0 {
        empty_list(
            ui,
            "No prompts match the filter",
            "Change the search or the mode filter.",
            "Clear filter",
            Action::ClearFilter,
            &mut actions,
        );
    } else {
        table(app, ui, &mut actions);
    }
    for action in actions {
        apply(app, ui.ctx(), action);
    }
}

fn empty_list(ui: &mut Ui, title: &str, text: &str, button: &str, action: Action, actions: &mut Vec<Action>) {
    ui.add_space(28.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(icons::HISTORY).size(26.0).color(Palette::TEXT_FAINT));
        ui.add_space(4.0);
        ui.label(RichText::new(title).size(15.0).color(Palette::TEXT));
        ui.label(RichText::new(text).color(Palette::TEXT_DIM));
        ui.add_space(6.0);
        if ui.button(button).clicked() {
            actions.push(action);
        }
    });
}

fn table(app: &mut AppState, ui: &mut Ui, actions: &mut Vec<Action>) {
    let st = &mut app.history;
    let scroll_to = if std::mem::take(&mut st.scroll_to_selected) { st.selected_row() } else { None };
    let st = &*st;
    let selected = st.selected.as_deref();
    egui::Frame::new().inner_margin(Margin { left: 4, right: 4, top: 2, bottom: 0 }).show(ui, |ui| {
        // Blender lists hover subtly; the default hover fill is a bright widget grey.
        ui.visuals_mut().widgets.hovered.bg_fill = Color32::from_gray(60);
        let mut builder = TableBuilder::new(ui)
            .id_salt("history-table")
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .auto_shrink([false, false])
            .min_scrolled_height(0.0)
            .max_scroll_height(f32::INFINITY)
            .column(Column::exact(22.0))
            .column(Column::exact(112.0))
            .column(Column::exact(38.0))
            .column(Column::remainder().at_least(120.0).clip(true))
            .column(Column::exact(50.0))
            .column(Column::exact(56.0));
        if let Some(row) = scroll_to {
            builder = builder.scroll_to_row(row, None);
        }
        builder
            .header(22.0, |mut header| {
                let columns = [
                    (SortBy::Star, icons::STAR, "Sort by star", false),
                    (SortBy::Date, "Date", "Sort by date", false),
                    (SortBy::Mode, "Mode", "Sort by mode", false),
                    (SortBy::Title, "Title", "Sort by title", false),
                    (SortBy::Score, "Score", "Sort by the check score", true),
                    (SortBy::Speed, "tok/s", "Sort by generation speed", true),
                ];
                for (by, label, tip, right) in columns {
                    header.col(|ui| {
                        if let Some(action) = header_cell(ui, st, by, label, tip, right) {
                            actions.push(action);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_H, st.view.len(), |mut row| {
                    let Some(entry) = st.view.get(row.index()).and_then(|&i| st.entries.get(i)) else {
                        return;
                    };
                    let is_selected = selected == Some(entry.id.as_str());
                    row.set_selected(is_selected);
                    let dim = if is_selected { Palette::TEXT } else { Palette::TEXT_DIM };
                    row.col(|ui| {
                        let (glyph, color) =
                            if entry.starred { (icons::STAR, STAR) } else { ("☆", Palette::TEXT_FAINT) };
                        let star = ui
                            .add(Label::new(RichText::new(glyph).color(color)).sense(Sense::click()))
                            .on_hover_text(if entry.starred { "Unstar (S)" } else { "Star (S)" });
                        if star.clicked() {
                            actions.push(Action::ToggleStar(entry.id.clone()));
                        }
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(entry.short_date()).color(dim));
                    });
                    row.col(|ui| {
                        let color = if is_selected { Palette::TEXT } else { mode_color(entry.mode) };
                        ui.label(RichText::new(entry.mode.as_str()).color(color));
                    });
                    row.col(|ui| {
                        if entry.parent_id.is_some() {
                            ui.label(RichText::new("↪").color(dim)).on_hover_text("Follow-up fix of an earlier prompt");
                        }
                        ui.add(Label::new(RichText::new(&entry.title).color(Palette::TEXT)).truncate());
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| match entry.lint_score {
                            Some(score) => {
                                let color =
                                    if is_selected { Palette::TEXT } else { score_color(score, entry.lint_errors) };
                                let errors = entry.lint_errors;
                                let text =
                                    if errors > 0 { format!("{score} {}", icons::WARN) } else { score.to_string() };
                                ui.label(RichText::new(text).color(color)).on_hover_text(format!(
                                    "Check score {score}/100, {errors} error{}",
                                    if errors == 1 { "" } else { "s" }
                                ));
                            }
                            None => {
                                ui.label(RichText::new("-").color(Palette::TEXT_FAINT));
                            }
                        });
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| match &entry.timings {
                            Some(t) => {
                                ui.label(RichText::new(format!("{:.1}", t.predicted_per_second)).color(dim));
                            }
                            None => {
                                ui.label(RichText::new("-").color(Palette::TEXT_FAINT));
                            }
                        });
                    });
                    let resp = row.response();
                    if resp.double_clicked() {
                        actions.push(Action::Open(entry.id.clone()));
                    } else if resp.clicked() || resp.secondary_clicked() {
                        actions.push(Action::Select(entry.id.clone()));
                    }
                    resp.context_menu(|ui| row_menu(ui, entry, actions));
                });
            });
    });
}

fn header_cell(ui: &mut Ui, st: &HistoryState, by: SortBy, label: &str, tip: &str, right: bool) -> Option<Action> {
    let cell = ui.max_rect().expand2(egui::vec2(ui.spacing().item_spacing.x * 0.5, 2.0));
    ui.painter().rect_filled(cell, 0.0, Palette::SIDE);
    ui.painter().hline(cell.x_range(), cell.bottom(), Stroke::new(1.0_f32, Palette::OUTLINE));
    let active = st.sort == by;
    let text = if active {
        format!("{label} {}", if st.ascending { icons::UP } else { icons::DOWN })
    } else {
        label.to_string()
    };
    let color = if active { Palette::TEXT } else { Palette::TEXT_DIM };
    let layout = if right { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
    let clicked = ui
        .with_layout(layout, |ui| {
            ui.add(Label::new(RichText::new(text).small().color(color)).sense(Sense::click()))
                .on_hover_text(format!("{tip} (click again to reverse)"))
                .clicked()
        })
        .inner;
    clicked.then_some(Action::Sort(by))
}

fn row_menu(ui: &mut Ui, entry: &HistoryEntry, actions: &mut Vec<Action>) {
    ui.set_min_width(180.0);
    let id = entry.id.clone();
    let items = [
        ("Open in Write", "Enter", Action::Open(id.clone())),
        ("Copy prompt", "Ctrl+C", Action::CopyPrompt(id.clone())),
        ("Copy request", "", Action::CopyRequest(id.clone())),
        (if entry.starred { "Unstar" } else { "Star" }, "S", Action::ToggleStar(id.clone())),
        ("Export .txt...", "", Action::Export(id.clone())),
    ];
    for (label, keys, action) in items {
        if ui.add(egui::Button::new(label).shortcut_text(keys)).clicked() {
            actions.push(action);
            ui.close_menu();
        }
    }
    ui.separator();
    if ui.add(egui::Button::new(RichText::new("Delete...").color(Palette::ERR)).shortcut_text("Del")).clicked() {
        actions.push(Action::AskDelete(id));
        ui.close_menu();
    }
}

// ---------------------------------------------------------------------------------------------
// Centre bottom: preview
// ---------------------------------------------------------------------------------------------

fn preview_area(app: &mut AppState, ui: &mut Ui) {
    let mut actions = Vec::new();
    let monospace = app.config.monospace_prompts;
    let st = &app.history;
    let entry = st.selected_entry();
    let has = entry.is_some();
    let starred = entry.is_some_and(|e| e.starred);
    let id = entry.map(|e| e.id.clone()).unwrap_or_default();
    widgets::area_header(ui, "Preview", |ui| {
        let delete = ui
            .add_enabled(has, egui::Button::new(icons::TRASH).min_size(egui::vec2(24.0, 22.0)))
            .on_hover_text("Delete this prompt (Delete)");
        if delete.clicked() {
            actions.push(Action::AskDelete(id.clone()));
        }
        let export = ui
            .add_enabled(has, egui::Button::new(format!("{} Export .txt", icons::SAVE)))
            .on_hover_text("Save the prompt as a text file (UTF-8, opens in Notepad)");
        if export.clicked() {
            actions.push(Action::Export(id.clone()));
        }
        let open = ui
            .add_enabled(has, egui::Button::new(format!("{} Open in Write", icons::WRITE)))
            .on_hover_text("Edit the request or fix the prompt in the Write workspace (Enter or double-click)");
        if open.clicked() {
            actions.push(Action::Open(id.clone()));
        }
        let copy = ui
            .add_enabled(has, egui::Button::new(format!("{} Copy prompt", icons::COPY)))
            .on_hover_text("Copy the prompt to the clipboard (Ctrl+C)");
        if copy.clicked() {
            actions.push(Action::CopyPrompt(id.clone()));
        }
        let star_text = if starred {
            RichText::new(format!("{} Starred", icons::STAR)).color(STAR)
        } else {
            RichText::new(format!("{} Star", icons::STAR))
        };
        let star = ui.add_enabled(has, egui::Button::new(star_text)).on_hover_text("Star or unstar this prompt (S)");
        if star.clicked() {
            actions.push(Action::ToggleStar(id.clone()));
        }
    });
    match entry {
        Some(entry) => {
            let parent_title = entry.parent_id.as_deref().and_then(|p| st.entry(p)).map(|p| p.title.as_str());
            egui::Frame::new().inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
                preview_body(ui, entry, parent_title, monospace, &mut actions);
            });
        }
        None => {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Select a prompt in the list to preview it.").color(Palette::TEXT_DIM));
            });
        }
    }
    for action in actions {
        apply(app, ui.ctx(), action);
    }
}

fn preview_body(
    ui: &mut Ui,
    entry: &HistoryEntry,
    parent_title: Option<&str>,
    monospace: bool,
    actions: &mut Vec<Action>,
) {
    ui.horizontal(|ui| {
        widgets::badge(ui, entry.mode.as_str(), mode_color(entry.mode)).on_hover_text(entry.mode.label());
        if entry.starred {
            ui.label(RichText::new(icons::STAR).color(STAR));
        }
        ui.add(Label::new(RichText::new(&entry.title).strong().size(14.5).color(Palette::TEXT)).truncate());
    });
    ui.add_space(2.0);
    meta_line(ui, entry, parent_title, actions);
    if !entry.reasoning.trim().is_empty() {
        ui.add_space(2.0);
        let words = entry.reasoning.split_whitespace().count();
        egui::CollapsingHeader::new(
            RichText::new(format!("{} Thinking ({words} words)", icons::THINK)).color(Palette::TEXT_DIM),
        )
        .id_salt(("history-thinking", entry.id.as_str()))
        .default_open(false)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("history-thinking-scroll", entry.id.as_str()))
                .max_height(130.0)
                .show(ui, |ui| {
                    ui.add(
                        Label::new(RichText::new(entry.reasoning.trim()).monospace().color(Palette::TEXT_DIM))
                            .selectable(true)
                            .wrap(),
                    );
                });
        });
    }
    ui.add_space(6.0);

    let height = ui.available_height();
    let width = ui.available_width();
    if width >= SIDE_BY_SIDE_MIN {
        let gap = 10.0;
        let column = (width - gap) / 2.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(egui::vec2(column, height), Layout::top_down(Align::Min), |ui| {
                request_block(ui, entry, height);
            });
            ui.allocate_ui_with_layout(egui::vec2(column, height), Layout::top_down(Align::Min), |ui| {
                prompt_block(ui, entry, height, monospace);
            });
        });
    } else {
        // Stacked: the (usually short) request gets less room than the prompt.
        let request_height = ((height - 8.0) * 0.38).max(70.0);
        let prompt_height = (height - 8.0 - request_height).max(70.0);
        request_block(ui, entry, request_height);
        ui.add_space(8.0);
        prompt_block(ui, entry, prompt_height, monospace);
    }
}

/// Settings, speed, check result, length and the follow-up link, in one wrapped line.
/// `parent_title`: title of the entry this one fixed, when it is still in the history.
fn meta_line(ui: &mut Ui, entry: &HistoryEntry, parent_title: Option<&str>, actions: &mut Vec<Action>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let dim = |text: String| RichText::new(text).small().color(Palette::TEXT_DIM);
        let dot = |ui: &mut Ui| {
            ui.label(RichText::new("·").small().color(Palette::TEXT_FAINT));
        };
        ui.label(dim(entry.short_date())).on_hover_text(format!("Id {}", entry.id));
        dot(ui);
        ui.label(dim(format!("Thinking {}", entry.params.thinking.as_str())));
        dot(ui);
        ui.label(dim(format!("Temperature {:.2}", entry.params.temperature)));
        if let Some(seconds) = entry.duration_s {
            dot(ui);
            ui.label(dim(format!("{seconds} s clip")));
        }
        if let Some(score) = entry.lint_score {
            dot(ui);
            let errors = entry.lint_errors;
            let text = match errors {
                0 => format!("Score {score}, no errors"),
                1 => format!("Score {score}, 1 error"),
                n => format!("Score {score}, {n} errors"),
            };
            ui.label(RichText::new(text).small().color(score_color(score, errors)))
                .on_hover_text("Result of the prompt check when it was written");
        }
        if let Some(t) = &entry.timings {
            dot(ui);
            let secs = (t.prompt_ms + t.predicted_ms) / 1000.0;
            ui.label(dim(format!("{} tokens at {:.1} tok/s, {:.1} s", t.predicted_n, t.predicted_per_second, secs)))
                .on_hover_text(format!(
                    "Prompt: {} new + {} cached tokens ({:.0} tok/s)\nAnswer: {} tokens ({:.1} tok/s)",
                    t.prompt_n, t.cache_n, t.prompt_per_second, t.predicted_n, t.predicted_per_second
                ));
        }
        if entry.spec.is_some() {
            dot(ui);
            ui.label(dim("builder form saved".to_string()))
                .on_hover_text("Open in Write restores the request builder form");
        }
        if let Some(parent) = &entry.parent_id {
            dot(ui);
            ui.label(dim("fix of".to_string()));
            let link = match parent_title {
                Some(title) => ui
                    .link(RichText::new(format!("\u{201c}{}\u{201d}", ellipsize(title, 40))).small())
                    .on_hover_text(format!("Select the prompt this one fixed ({parent})")),
                None => ui.link(RichText::new(parent).small()).on_hover_text("That prompt is no longer in the history"),
            };
            if link.clicked() {
                actions.push(Action::SelectParent(parent.clone()));
            }
        }
    });
}

fn request_block(ui: &mut Ui, entry: &HistoryEntry, height: f32) {
    let request = entry.original_request();
    block_header(ui, "Request", request, |ui| {
        if let Some(fix) = entry.fix_text() {
            widgets::badge(ui, &format!("Fix: {}", ellipsize(fix.trim(), 48)), Palette::INFO)
                .on_hover_text(format!("Follow-up fix sent after the earlier answer:\n{}", fix.trim()));
        }
    });
    readonly_text(ui, ("history-request", entry.id.as_str()), request, height - 26.0, true);
}

fn prompt_block(ui: &mut Ui, entry: &HistoryEntry, height: f32, monospace: bool) {
    block_header(ui, "Prompt", &entry.output, |_| {});
    readonly_text(ui, ("history-prompt", entry.id.as_str()), &entry.output, height - 26.0, monospace);
}

/// Section label, extra badges, and the word count on the right.
fn block_header(ui: &mut Ui, title: &str, text: &str, extra: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(20.0);
        widgets::section_label(ui, title);
        extra(ui);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let words = text.split_whitespace().count();
            ui.label(RichText::new(format!("{words} words")).small().color(Palette::TEXT_FAINT));
        });
    });
}

/// Read-only text (selectable, Ctrl+C copies the selection) on the dark input background, in a
/// fixed-height scroll area.
fn readonly_text(ui: &mut Ui, id_salt: impl std::hash::Hash, text: &str, height: f32, monospace: bool) {
    let height = height.max(40.0);
    egui::Frame::new()
        .fill(Palette::INPUT)
        .stroke(Stroke::new(1.0_f32, Palette::OUTLINE))
        .corner_radius(CornerRadius::same(4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt(id_salt).max_height(height - 2.0).auto_shrink([false, false]).show(
                ui,
                |ui| {
                    let mut view = text;
                    let style = if monospace { TextStyle::Monospace } else { TextStyle::Body };
                    ui.add(
                        egui::TextEdit::multiline(&mut view)
                            .font(style)
                            .frame(false)
                            .desired_width(f32::INFINITY)
                            .margin(Margin::symmetric(8, 6)),
                    );
                },
            );
        });
}

// ---------------------------------------------------------------------------------------------
// Confirmation and helpers
// ---------------------------------------------------------------------------------------------

fn confirm_dialog(app: &mut AppState, ctx: &egui::Context) {
    let Some(id) = app.history.confirm_delete.clone() else {
        return;
    };
    let title = app.history.entry(&id).map(|e| e.title.clone()).unwrap_or_else(|| id.clone());
    let body =
        format!("\u{201c}{}\u{201d} is removed from the history folder. This cannot be undone.", ellipsize(&title, 60));
    match confirm_popup(ctx, "history-confirm", "Delete this prompt?", &body, "Delete") {
        Some(true) => {
            app.history.confirm_delete = None;
            delete_entry(app, &id);
        }
        Some(false) => app.history.confirm_delete = None,
        None => {}
    }
}

fn mode_color(mode: Mode) -> Color32 {
    match mode {
        Mode::R2v => Palette::INFO,
        Mode::F2v => Color32::from_rgb(120, 190, 160),
    }
}

fn score_color(score: u8, errors: usize) -> Color32 {
    if errors > 0 {
        Palette::ERR
    } else if score >= 85 {
        Palette::OK
    } else if score >= 60 {
        Palette::WARN
    } else {
        Palette::ERR
    }
}

fn ellipsize(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
        None => text.to_string(),
    }
}

fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, mode: Mode, title: &str, score: Option<u8>, starred: bool) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            mode,
            title: title.into(),
            request: format!("request {title}"),
            output: format!("prompt {title}"),
            lint_score: score,
            starred,
            ..HistoryEntry::default()
        }
    }

    fn state() -> HistoryState {
        HistoryState {
            entries: vec![
                entry("3", Mode::R2v, "beta", Some(70), false),
                entry("2", Mode::F2v, "Alpha", Some(95), true),
                entry("1", Mode::R2v, "gamma", None, false),
            ],
            generation: 1,
            ..HistoryState::default()
        }
    }

    fn ids(st: &HistoryState) -> Vec<&str> {
        st.view.iter().map(|&i| st.entries[i].id.as_str()).collect()
    }

    #[test]
    fn view_filters_and_sorts() {
        let mut st = state();
        rebuild_view(&mut st);
        assert_eq!(ids(&st), ["3", "2", "1"]);
        st.mode = ModeFilter::R2v;
        rebuild_view(&mut st);
        assert_eq!(ids(&st), ["3", "1"]);
        st.mode = ModeFilter::All;
        st.starred_only = true;
        rebuild_view(&mut st);
        assert_eq!(ids(&st), ["2"]);
        st.starred_only = false;
        st.query = "GAMMA".into();
        rebuild_view(&mut st);
        assert_eq!(ids(&st), ["1"]);
        st.query.clear();
        st.sort = SortBy::Title;
        st.ascending = true;
        rebuild_view(&mut st);
        assert_eq!(ids(&st), ["2", "3", "1"]);
        st.sort = SortBy::Score;
        st.ascending = false;
        rebuild_view(&mut st);
        assert_eq!(ids(&st), ["2", "3", "1"]);
        st.ascending = true;
        rebuild_view(&mut st);
        assert_eq!(ids(&st), ["1", "3", "2"]);
    }

    #[test]
    fn an_entry_already_deleted_on_disk_can_still_be_removed() {
        let dir = std::env::temp_dir().join(format!("bh3-history-ws-test-{}", std::process::id()));
        let saved = entry("20261028-101010", Mode::R2v, "gone", None, false);
        history::save_in(&dir, &saved).unwrap();
        assert!(delete_file(&dir, &saved.id).unwrap());
        // Deleted again (as after `bh3 history delete` or Explorer): gone, not an error.
        assert!(!delete_file(&dir, &saved.id).unwrap());
        // A file that is there but cannot be removed still fails (here a folder with its name).
        std::fs::create_dir_all(history::entry_path_in(&dir, "20261028-101011")).unwrap();
        assert!(delete_file(&dir, "20261028-101011").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn selection_moves_within_the_view() {
        let mut st = state();
        rebuild_view(&mut st);
        move_selection(&mut st, 1);
        assert_eq!(st.selected.as_deref(), Some("3"));
        move_selection(&mut st, 1);
        move_selection(&mut st, 5);
        assert_eq!(st.selected.as_deref(), Some("1"));
        move_selection(&mut st, -1);
        assert_eq!(st.selected.as_deref(), Some("2"));
    }
}
