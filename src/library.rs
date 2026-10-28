//! Built-in texts (system prompts, templates, fixes, checklist, guides) with user overrides stored
//! in `config_dir()\library\<file_name>`. The defaults are the files in `assets/library/`, tested
//! on the user's Bonsai 27B (thinking off, temperature 0.4).
//!
//! Override files are read as UTF-8 (a BOM is stripped; UTF-16 files with a BOM are decoded too) and
//! line endings are normalised to `\n`, so the text sent to Bonsai stays byte-identical between runs
//! (llama-server only reuses its prompt cache for an identical system prompt). The built-in texts
//! are normalised the same way, so a CRLF checkout of the assets behaves like an LF one.
//!
//! [`Library::load_from`], [`Library::set_text_in`] and [`Library::reset_in`] take the library
//! folder explicitly (tests, portable use); the plain versions use [`library_dir`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::gen::Mode;

/// Editable library entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryItem {
    SystemR2v,
    SystemF2v,
    Templates,
    Fixes,
    Checklist,
    GuideR2v,
    GuideF2v,
}

impl LibraryItem {
    /// Every item, in display order.
    pub fn all() -> &'static [LibraryItem] {
        &[
            LibraryItem::SystemR2v,
            LibraryItem::SystemF2v,
            LibraryItem::Templates,
            LibraryItem::Fixes,
            LibraryItem::Checklist,
            LibraryItem::GuideR2v,
            LibraryItem::GuideF2v,
        ]
    }
    /// UI label.
    pub fn label(&self) -> &'static str {
        match self {
            LibraryItem::SystemR2v => "System prompt - r2v",
            LibraryItem::SystemF2v => "System prompt - f2v",
            LibraryItem::Templates => "Request templates",
            LibraryItem::Fixes => "Follow-up fixes",
            LibraryItem::Checklist => "Checklist",
            LibraryItem::GuideR2v => "Guide - r2v (images)",
            LibraryItem::GuideF2v => "Guide - f2v (first frame)",
        }
    }
    /// File name of the built-in asset and of the override file.
    pub fn file_name(&self) -> &'static str {
        match self {
            LibraryItem::SystemR2v => "system_r2v.txt",
            LibraryItem::SystemF2v => "system_f2v.txt",
            LibraryItem::Templates => "templates.txt",
            LibraryItem::Fixes => "fixes.txt",
            LibraryItem::Checklist => "checklist.txt",
            LibraryItem::GuideR2v => "guide_r2v.md",
            LibraryItem::GuideF2v => "guide_f2v.md",
        }
    }
    /// Built-in text.
    pub fn default_text(&self) -> &'static str {
        match self {
            LibraryItem::SystemR2v => include_str!("../assets/library/system_r2v.txt"),
            LibraryItem::SystemF2v => include_str!("../assets/library/system_f2v.txt"),
            LibraryItem::Templates => include_str!("../assets/library/templates.txt"),
            LibraryItem::Fixes => include_str!("../assets/library/fixes.txt"),
            LibraryItem::Checklist => include_str!("../assets/library/checklist.txt"),
            LibraryItem::GuideR2v => include_str!("../assets/library/guide_r2v.md"),
            LibraryItem::GuideF2v => include_str!("../assets/library/guide_f2v.md"),
        }
    }
    /// True for the two system prompts (sent to Bonsai).
    pub fn is_system_prompt(&self) -> bool {
        matches!(self, LibraryItem::SystemR2v | LibraryItem::SystemF2v)
    }
}

impl LibraryItem {
    /// Short kebab-case id for the CLI, e.g. `system-r2v`, `templates`, `guide-f2v`.
    pub fn id(&self) -> &'static str {
        match self {
            LibraryItem::SystemR2v => "system-r2v",
            LibraryItem::SystemF2v => "system-f2v",
            LibraryItem::Templates => "templates",
            LibraryItem::Fixes => "fixes",
            LibraryItem::Checklist => "checklist",
            LibraryItem::GuideR2v => "guide-r2v",
            LibraryItem::GuideF2v => "guide-f2v",
        }
    }

    /// Item from its id (`system-r2v`), serde name (`system_r2v`) or file name (`system_r2v.txt`),
    /// ignoring case.
    pub fn from_id(name: &str) -> Option<LibraryItem> {
        let key: String =
            name.trim().to_ascii_lowercase().chars().map(|c| if c == '_' || c == ' ' { '-' } else { c }).collect();
        let key = key.strip_suffix(".txt").or_else(|| key.strip_suffix(".md")).unwrap_or(&key);
        LibraryItem::all().iter().copied().find(|item| item.id() == key)
    }

    /// What the item is for (shown next to the editor).
    pub fn description(&self) -> &'static str {
        match self {
            LibraryItem::SystemR2v => {
                "Sent to Bonsai as the system prompt for r2v (reference images). Keep it unchanged \
                 between requests so llama-server can reuse its prompt cache."
            }
            LibraryItem::SystemF2v => {
                "Sent to Bonsai as the system prompt for f2v (first frame, optionally first + last \
                 frame). Keep it unchanged between requests so llama-server can reuse its prompt cache."
            }
            LibraryItem::Templates => {
                "Request templates listed in the Write workspace (a click fills the request). \
                 \"==== r2v ... ====\" / \"==== f2v ... ====\" headings pick the mode, \"--- title ---\" \
                 starts a template; other sections are ignored."
            }
            LibraryItem::Fixes => {
                "One-line follow-up fixes offered after an answer, one per line. \"==== r2v ... ====\" / \
                 \"==== f2v ... ====\" headings mark fixes for one mode, other headings fixes for both. \
                 The header paragraph and the closing note are not fixes."
            }
            LibraryItem::Checklist => {
                "Checks before pasting the prompt into ComfyUI. Lines starting with \"[ ]\" are items, \
                 indented lines continue the item above, other lines start a section."
            }
            LibraryItem::GuideR2v => "Readable short guide for r2v prompts (not sent to Bonsai).",
            LibraryItem::GuideF2v => "Readable short guide for f2v prompts (not sent to Bonsai).",
        }
    }
}

/// A request template parsed from the templates text (`--- title ---` blocks under
/// `==== r2v ... ====` / `==== f2v ... ====` headings).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    /// kebab-case id from the title, e.g. `person-place-photo`.
    pub id: String,
    pub title: String,
    pub mode: Mode,
    /// The template body (with `<placeholders>`).
    pub text: String,
}

/// Library texts currently in use.
#[derive(Debug, Clone, PartialEq)]
pub struct Library {
    texts: HashMap<LibraryItem, String>,
    overridden: HashMap<LibraryItem, bool>,
}

impl Default for Library {
    fn default() -> Self {
        Self::builtin()
    }
}

/// `config_dir()\library`, where override files live (not created by this call).
pub fn library_dir() -> PathBuf {
    crate::config::config_dir().join("library")
}

/// Path of the override file of `item` inside `dir`.
pub fn override_path_in(dir: &Path, item: LibraryItem) -> PathBuf {
    dir.join(item.file_name())
}

/// Path of the override file of `item` in [`library_dir`] (it may not exist).
pub fn override_path(item: LibraryItem) -> PathBuf {
    override_path_in(&library_dir(), item)
}

impl Library {
    /// Built-in texts only (no disk access).
    pub fn builtin() -> Self {
        let texts = LibraryItem::all().iter().map(|&item| (item, builtin_text(item))).collect();
        let overridden = LibraryItem::all().iter().map(|&item| (item, false)).collect();
        Self { texts, overridden }
    }

    /// Built-ins replaced by any override files in `config_dir()\library`.
    pub fn load() -> Self {
        Self::load_from(&library_dir())
    }

    /// Built-ins replaced by any override files in `dir`. Missing, unreadable or blank files
    /// keep the built-in text. An override equal to the built-in does not count as overridden.
    pub fn load_from(dir: &Path) -> Self {
        let mut library = Self::builtin();
        for &item in LibraryItem::all() {
            let Some(text) = read_override(&override_path_in(dir, item)) else {
                continue;
            };
            let differs = !same_text(&text, &builtin_text(item));
            library.texts.insert(item, text);
            library.overridden.insert(item, differs);
        }
        library
    }

    /// Current text of an item.
    pub fn text(&self, item: LibraryItem) -> &str {
        self.texts.get(&item).map(String::as_str).unwrap_or_else(|| item.default_text())
    }

    /// Replace a text and save it as an override file.
    ///
    /// Line endings are normalised to `\n`. A blank text, or one equal to the built-in text,
    /// resets the item instead (deletes the override file).
    pub fn set_text(&mut self, item: LibraryItem, text: String) -> anyhow::Result<()> {
        self.set_text_in(&library_dir(), item, text)
    }

    /// [`Library::set_text`] with the override folder given explicitly.
    pub fn set_text_in(&mut self, dir: &Path, item: LibraryItem, text: String) -> anyhow::Result<()> {
        let text = normalize_text(&text);
        if text.trim().is_empty() || same_text(&text, &builtin_text(item)) {
            return self.reset_in(dir, item);
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = override_path_in(dir, item);
        crate::history::write_atomic(&path, text.as_bytes()).with_context(|| format!("saving {}", item.label()))?;
        self.texts.insert(item, text);
        self.overridden.insert(item, true);
        Ok(())
    }

    /// Go back to the built-in text (deletes the override file).
    pub fn reset(&mut self, item: LibraryItem) -> anyhow::Result<()> {
        self.reset_in(&library_dir(), item)
    }

    /// [`Library::reset`] with the override folder given explicitly.
    pub fn reset_in(&mut self, dir: &Path, item: LibraryItem) -> anyhow::Result<()> {
        let path = override_path_in(dir, item);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("deleting {}", path.display())),
        }
        self.texts.insert(item, builtin_text(item));
        self.overridden.insert(item, false);
        Ok(())
    }

    /// True when the item's text differs from the built-in one (an override is in use).
    pub fn is_overridden(&self, item: LibraryItem) -> bool {
        self.overridden.get(&item).copied().unwrap_or(false)
    }

    /// System prompt for a mode.
    pub fn system_prompt(&self, mode: Mode) -> &str {
        match mode {
            Mode::R2v => self.text(LibraryItem::SystemR2v),
            Mode::F2v => self.text(LibraryItem::SystemF2v),
        }
    }

    /// Readable guide for a mode.
    pub fn guide(&self, mode: Mode) -> &str {
        match mode {
            Mode::R2v => self.text(LibraryItem::GuideR2v),
            Mode::F2v => self.text(LibraryItem::GuideF2v),
        }
    }

    /// Parsed request templates (both modes).
    pub fn templates(&self) -> Vec<Template> {
        parse_templates(self.text(LibraryItem::Templates))
    }

    /// One-line follow-up fixes of both modes (the lines of the fixes text that are fixes, not
    /// headings/notes).
    pub fn fixes(&self) -> Vec<String> {
        parse_fixes(self.text(LibraryItem::Fixes))
    }

    /// Follow-up fixes that suit `mode`: those under a heading of that mode or of both modes
    /// (fixes written for the other mode are left out); see [`parse_fixes_by_mode`].
    pub fn fixes_for(&self, mode: Mode) -> Vec<String> {
        parse_fixes_by_mode(self.text(LibraryItem::Fixes))
            .into_iter()
            .filter(|(fix_mode, _)| fix_mode.is_none_or(|m| m == mode))
            .map(|(_, fix)| fix)
            .collect()
    }

    /// Checklist as (section, item) pairs from the `[ ]` lines.
    pub fn checklist_items(&self) -> Vec<(String, String)> {
        parse_checklist(self.text(LibraryItem::Checklist))
    }
}

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Strip a leading BOM and normalise CRLF / lone CR line endings to `\n`.
pub(crate) fn normalize_text(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_string()
    }
}

/// The normalised built-in text of an item.
fn builtin_text(item: LibraryItem) -> String {
    normalize_text(item.default_text())
}

/// Equal apart from trailing whitespace (an editor dropping the final newline is no edit).
fn same_text(a: &str, b: &str) -> bool {
    a.trim_end() == b.trim_end()
}

/// Read an override file; `None` when it is missing, unreadable or blank.
fn read_override(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = normalize_text(&decode_text(&bytes));
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Bytes to text: UTF-8 (BOM optional), UTF-16 LE/BE with BOM; invalid UTF-8 is decoded lossily.
fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|pair| unit(*pair));
    char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}

// ---------------------------------------------------------------------------------------------
// Parsers (public so editors can preview unsaved text)
// ---------------------------------------------------------------------------------------------

/// A template block being collected.
struct PendingTemplate<'a> {
    title: String,
    mode: Mode,
    lines: Vec<&'a str>,
}

/// Parse request templates from a templates text.
///
/// `==== <heading> ====` lines start a section whose mode comes from the heading (`r2v`, `f2v`;
/// sections without a mode, such as `length words`, are skipped). `--- <title> ---` lines start a
/// template; its text runs until the next template or heading. Lines before the first template
/// (the intro) are ignored; templates before the first heading count as r2v. Empty templates are
/// skipped and duplicate ids get `-2`, `-3` suffixes.
pub fn parse_templates(text: &str) -> Vec<Template> {
    let text = normalize_text(text);
    let mut out: Vec<Template> = Vec::new();
    let mut section: Option<Mode> = Some(Mode::R2v);
    let mut current: Option<PendingTemplate<'_>> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("===") {
            finish_template(current.take(), &mut out);
            let heading = trimmed.trim_matches('=').trim();
            if !heading.is_empty() {
                section = heading_mode(heading);
            }
            continue;
        }
        if trimmed.starts_with("---") {
            finish_template(current.take(), &mut out);
            let title = trimmed.trim_matches('-').trim();
            if !title.is_empty() {
                current = section.map(|mode| PendingTemplate { title: title.to_string(), mode, lines: Vec::new() });
            }
            continue;
        }
        if let Some(pending) = current.as_mut() {
            pending.lines.push(line.trim_end());
        }
    }
    finish_template(current.take(), &mut out);
    out
}

fn finish_template(pending: Option<PendingTemplate<'_>>, out: &mut Vec<Template>) {
    let Some(pending) = pending else {
        return;
    };
    let text = pending.lines.join("\n").trim().to_string();
    if text.is_empty() {
        return;
    }
    let base = template_id(&pending.title);
    let mut id = base.clone();
    let mut n = 2;
    while out.iter().any(|t| t.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    out.push(Template { id, title: pending.title, mode: pending.mode, text });
}

/// Words in a section heading that name a mode.
const MODE_KEYS: [(&str, Mode); 7] = [
    ("r2v", Mode::R2v),
    ("reference", Mode::R2v),
    ("f2v", Mode::F2v),
    ("i2v", Mode::F2v),
    ("fl2v", Mode::F2v),
    ("first frame", Mode::F2v),
    ("first-frame", Mode::F2v),
];

/// Mode named by a section heading (`r2v (use system prompt 2a)` -> r2v), `None` for other sections.
fn heading_mode(heading: &str) -> Option<Mode> {
    let lower = heading.to_lowercase();
    MODE_KEYS
        .iter()
        .filter_map(|&(key, mode)| lower.find(key).map(|pos| (pos, mode)))
        .min_by_key(|&(pos, _)| pos)
        .map(|(_, mode)| mode)
}

/// Mode of a fixes section heading: the one mode it names, `None` (both modes) when it names
/// both (`both modes`, `r2v and f2v`) or neither.
fn fix_heading_mode(heading: &str) -> Option<Mode> {
    let lower = heading.to_lowercase();
    let named = |mode: Mode| MODE_KEYS.iter().any(|&(key, m)| m == mode && lower.contains(key));
    match (named(Mode::R2v), named(Mode::F2v)) {
        (true, false) => Some(Mode::R2v),
        (false, true) => Some(Mode::F2v),
        _ => None,
    }
}

/// kebab-case id from a title; parenthetical remarks are left out when something else remains
/// (`first + last frame (turn the Last frame image ON in ComfyUI)` -> `first-last-frame`).
fn template_id(title: &str) -> String {
    let mut depth = 0usize;
    let mut outside = String::new();
    for c in title.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => outside.push(c),
            _ => {}
        }
    }
    let id = kebab(&outside);
    if !id.is_empty() {
        return id;
    }
    let id = kebab(title);
    if id.is_empty() {
        "template".to_string()
    } else {
        id
    }
}

fn kebab(text: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.extend(c.to_lowercase());
        } else {
            gap = true;
        }
    }
    out
}

/// Parse the follow-up fixes of both modes (see [`parse_fixes_by_mode`]).
pub fn parse_fixes(text: &str) -> Vec<String> {
    parse_fixes_by_mode(text).into_iter().map(|(_, fix)| fix).collect()
}

/// Parse the follow-up fixes with their mode (`None` = both modes): every non-empty line, except
/// - `==== <heading> ====` lines, which start a section in the style of the templates text: a
///   heading that names one mode (`r2v`, `f2v`) holds fixes for that mode, any other heading
///   (`both modes`) fixes for both; fixes before the first heading are for both modes,
/// - the intro paragraph when it opens with a title in capitals (`FOLLOW-UP FIXES - ...`);
///   when the whole text is one paragraph, only the title line is dropped,
/// - a closing one-line note paragraph (`If an answer goes badly wrong, ...`, `Note: ...`),
/// - comment lines starting with `#` or `//`.
///
/// List markers (`- `, `* `, `1. `) are removed.
pub fn parse_fixes_by_mode(text: &str) -> Vec<(Option<Mode>, String)> {
    let text = normalize_text(text);
    let mut paragraphs: Vec<Vec<(Option<Mode>, &str)>> = Vec::new();
    let mut current: Vec<(Option<Mode>, &str)> = Vec::new();
    let mut section: Option<Mode> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        let heading = trimmed.starts_with("===");
        if trimmed.is_empty() || heading {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
            if heading {
                section = fix_heading_mode(trimmed.trim_matches('=').trim());
            }
            continue;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        current.push((section, trimmed));
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }

    let opens_with_title = paragraphs.first().and_then(|p| p.first()).is_some_and(|(_, line)| is_caps_title(line));
    if opens_with_title {
        if paragraphs.len() >= 2 {
            paragraphs.remove(0);
        } else if let Some(only) = paragraphs.first_mut() {
            while only.first().is_some_and(|(_, line)| is_caps_title(line)) {
                only.remove(0);
            }
        }
    }
    if paragraphs.len() >= 2 {
        let closing_note = paragraphs.last().is_some_and(|p| p.len() == 1 && is_note_line(p[0].1));
        if closing_note {
            paragraphs.pop();
        }
    }

    paragraphs
        .into_iter()
        .flatten()
        .map(|(mode, line)| (mode, strip_list_marker(line)))
        .filter(|(_, line)| !line.is_empty())
        .map(|(mode, line)| (mode, line.to_string()))
        .collect()
}

/// Parse the checklist into (section, item) pairs.
///
/// Lines starting with `[ ]` (also `[x]`, `[]`, `- [ ]`) are items; indented lines right after an
/// item continue it (joined with a space); any other non-empty line names the section of the
/// following items (a trailing `:` is dropped).
pub fn parse_checklist(text: &str) -> Vec<(String, String)> {
    let text = normalize_text(text);
    let mut out: Vec<(String, String)> = Vec::new();
    let mut section = String::new();
    let mut in_item = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_item = false;
            continue;
        }
        if let Some(item) = checklist_item(trimmed) {
            out.push((section.clone(), item.to_string()));
            in_item = true;
            continue;
        }
        let indented = line.starts_with(char::is_whitespace);
        if indented && in_item {
            if let Some((_, last)) = out.last_mut() {
                if !last.is_empty() {
                    last.push(' ');
                }
                last.push_str(trimmed);
            }
            continue;
        }
        section = trimmed.trim_end_matches(':').trim_end().to_string();
        in_item = false;
    }
    out.retain(|(_, item)| !item.is_empty());
    out
}

/// Text of a checklist item line, `None` for other lines.
fn checklist_item(trimmed: &str) -> Option<&str> {
    let line =
        trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).map(str::trim_start).unwrap_or(trimmed);
    ["[ ]", "[x]", "[X]", "[]"].iter().find_map(|checkbox| line.strip_prefix(checkbox)).map(str::trim)
}

/// A title line in capitals: the part before ` - ` / `:` has 3+ letters, all upper case
/// (`FOLLOW-UP FIXES - send one ...`, `CHECKLIST BEFORE YOU PASTE ...`).
fn is_caps_title(line: &str) -> bool {
    let mut end = line.len();
    for sep in [" - ", " \u{2013} ", " \u{2014} ", ":"] {
        if let Some(pos) = line.find(sep) {
            end = end.min(pos);
        }
    }
    let letters: Vec<char> = line[..end].chars().filter(|c| c.is_alphabetic()).collect();
    letters.len() >= 3 && letters.iter().all(|c| c.is_uppercase())
}

/// Advice for the user rather than a fix (`If an answer goes badly wrong ...`, `Note: ...`).
fn is_note_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    ["if ", "note:", "notes:", "note -", "tip:", "tips:", "remember"].iter().any(|prefix| lower.starts_with(prefix))
}

/// Remove a list marker (`- `, `* `, `• `, `+ `, `1. `, `2) `).
fn strip_list_marker(line: &str) -> &str {
    let line = line.trim_start();
    for marker in ["- ", "* ", "\u{2022} ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest.trim_start();
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty temp folder for one test (never the real config dir).
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bh3-library-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn system_prompt_examples_pass_the_check() {
        use crate::lint::{self, LintContext};
        let lib = Library::builtin();
        // The first EXAMPLE of a system prompt, without its `EXAMPLE (user: ...)` line.
        let example = |mode: Mode| {
            let text = lib.system_prompt(mode);
            let body = &text[text.find("EXAMPLE (user:").expect("example")..];
            let body = &body[body.find('\n').expect("example line") + 1..];
            body[..body.find("\n\nSTRICT:").unwrap_or(body.len())].to_string()
        };
        // The examples teach Bonsai the format and the length: they pass the check without a note.
        let r2v_ctx = LintContext {
            mode: Mode::R2v,
            duration_s: Some(6.0),
            ref_count: Some(2),
            first_frame_pinned: Some(false),
            has_last_frame: None,
        };
        let r2v = lint::lint(&example(Mode::R2v), &r2v_ctx);
        assert!(r2v.issues.is_empty(), "{:#?}", r2v.issues);
        assert!((350..=500).contains(&r2v.description_words), "{}", r2v.description_words);
        let f2v_ctx = LintContext {
            mode: Mode::F2v,
            duration_s: Some(6.0),
            ref_count: Some(1),
            first_frame_pinned: None,
            has_last_frame: Some(false),
        };
        let f2v = lint::lint(&example(Mode::F2v), &f2v_ctx);
        assert!(f2v.issues.is_empty(), "{:#?}", f2v.issues);
        assert!((150..=300).contains(&f2v.description_words), "{}", f2v.description_words);
        // Both length tables name every length the builder offers, with its real length.
        for d in crate::lengths::DURATIONS {
            let entry = format!("{d}s={}", crate::lengths::format_real(d as f32));
            for mode in [Mode::R2v, Mode::F2v] {
                assert!(lib.system_prompt(mode).contains(&entry), "{entry} missing for {mode}");
            }
            assert!(lib.text(LibraryItem::Checklist).contains(&entry), "{entry} missing from the checklist");
        }
    }

    #[test]
    fn builtin_has_every_item_unmodified() {
        let lib = Library::builtin();
        for &item in LibraryItem::all() {
            assert!(!lib.is_overridden(item), "{item:?}");
            assert_eq!(lib.text(item), normalize_text(item.default_text()));
            assert!(!lib.text(item).contains('\r'));
            assert!(!lib.text(item).trim().is_empty());
        }
        assert!(lib.system_prompt(Mode::R2v).starts_with("You write MiniMax H3 reference-to-video prompts"));
        assert!(lib.system_prompt(Mode::F2v).starts_with("You write MiniMax H3 first-frame video prompts"));
        assert!(lib.guide(Mode::R2v).starts_with("# H3 r2v prompt guide"));
        assert!(lib.guide(Mode::F2v).starts_with("# How to write the f2v prompt"));
        assert_eq!(Library::default(), lib);
    }

    #[test]
    fn item_ids_round_trip() {
        for &item in LibraryItem::all() {
            assert_eq!(LibraryItem::from_id(item.id()), Some(item));
            assert_eq!(LibraryItem::from_id(item.file_name()), Some(item));
            assert!(!item.description().is_empty());
        }
        assert_eq!(LibraryItem::from_id("SYSTEM_R2V"), Some(LibraryItem::SystemR2v));
        assert_eq!(LibraryItem::from_id("guide f2v"), Some(LibraryItem::GuideF2v));
        assert_eq!(LibraryItem::from_id("nope"), None);
    }

    #[test]
    fn builtin_templates_parse() {
        let templates = Library::builtin().templates();
        let r2v: Vec<_> = templates.iter().filter(|t| t.mode == Mode::R2v).collect();
        let f2v: Vec<_> = templates.iter().filter(|t| t.mode == Mode::F2v).collect();
        assert_eq!(r2v.len(), 10, "{r2v:#?}");
        assert_eq!(f2v.len(), 2, "{f2v:#?}");

        let first = &templates[0];
        assert_eq!(first.id, "1-photo-of-a-person");
        assert_eq!(first.title, "1 photo of a person");
        assert_eq!(
            first.text,
            "Ref 1: <age, hair, face, clothes>.\nPlace: <where the video happens, light, time of day>.\n\
             <6> s, <one take / N cuts>: <what she does, one continuous action>. Style: <film look / natural / ...>."
        );
        let ids: Vec<&str> = templates.iter().map(|t| t.id.as_str()).collect();
        for expected in [
            "person-place-photo",
            "style-mood-from-a-photo",
            "ref-1-is-the-exact-first-frame",
            "with-dialogue",
            "first-frame-only",
            "first-last-frame",
        ] {
            assert!(ids.contains(&expected), "{expected} missing from {ids:?}");
        }
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids must be unique");

        let pinned = templates.iter().find(|t| t.id == "ref-1-is-the-exact-first-frame").map(|t| t.title.as_str());
        assert_eq!(pinned, Some("Ref 1 is the exact first frame (turn the First-frame pin group ON in ComfyUI)"));

        // Section boundaries: no heading, intro or "length words" text leaks into a template.
        for t in &templates {
            assert!(!t.text.contains("===="), "{}", t.id);
            assert!(!t.text.contains("---"), "{}", t.id);
            assert!(!t.text.contains("quick test"), "{}", t.id);
            assert!(!t.text.contains("REQUEST TEMPLATES"), "{}", t.id);
            assert_eq!(t.text.trim(), t.text, "{}", t.id);
        }
        let dialogue = templates.iter().find(|t| t.id == "with-dialogue").map(|t| t.text.as_str());
        assert_eq!(
            dialogue,
            Some("... <she> says \"<exact words>\". <he> answers \"<exact words>\". (Bonsai adds the (S1)/(S2) IDs and <d> tags.)")
        );
        let last = templates.last().map(|t| (t.id.as_str(), t.mode, t.text.as_str()));
        assert_eq!(
            last,
            Some((
                "first-last-frame",
                Mode::F2v,
                "Picture 1: <the first frame>. Picture 2: <the last frame>.\n<6> s, one shot: <how she gets from the first to the last frame>."
            ))
        );
    }

    #[test]
    fn templates_parse_crlf_and_user_variants() {
        let text = "\u{feff}intro line\r\n--- loose block ---\r\nbefore any heading\r\n\
                    ======== F2V templates ========\r\n--- Same Title ---\r\nA\r\n\r\n--- Same Title ---\r\nB\r\n\
                    --- empty ---\r\n\r\n=== notes ===\r\n--- skipped ---\r\nnot a template\r\n\
                    ==== Reference mode ====\r\n--- (only remarks) ---\r\nC\r\n";
        let templates = parse_templates(text);
        let summary: Vec<(&str, Mode, &str)> =
            templates.iter().map(|t| (t.id.as_str(), t.mode, t.text.as_str())).collect();
        assert_eq!(
            summary,
            vec![
                ("loose-block", Mode::R2v, "before any heading"),
                ("same-title", Mode::F2v, "A"),
                ("same-title-2", Mode::F2v, "B"),
                ("only-remarks", Mode::R2v, "C"),
            ]
        );
    }

    #[test]
    fn builtin_fixes_parse() {
        let lib = Library::builtin();
        let fixes = lib.fixes();
        assert_eq!(fixes.len(), 14, "{fixes:#?}");
        assert_eq!(fixes[0], "This is a single take: use only [Shot 1] and no times inside it.");
        assert!(fixes.contains(&"Remove the music (non_diegetic_music: N/A).".to_string()));
        for fix in &fixes {
            assert!(!fix.contains("FOLLOW-UP"), "{fix}");
            assert!(!fix.starts_with("Each costs") && !fix.starts_with("To switch"), "{fix}");
            assert!(!fix.starts_with("If an answer"), "{fix}");
            assert!(!fix.contains("===") && !fix.contains("Rewrite it as"), "{fix}");
        }

        // Each mode gets the shared fixes and its own, in file order; every fix fits its mode's rules.
        let (r2v, f2v) = (lib.fixes_for(Mode::R2v), lib.fixes_for(Mode::F2v));
        assert_eq!((r2v.len(), f2v.len()), (11, 8));
        assert_eq!(r2v[..5], f2v[..5], "the shared fixes come first");
        assert!(r2v.iter().chain(&f2v).all(|f| fixes.contains(f)));
        assert_eq!(
            r2v[5],
            "Make the detailed_description longer (about 350 words): add light, fabric, hair movement, small gestures and the camera."
        );
        for fix in &f2v {
            for r2v_only in
                ["detailed_description", "<Subject", "partially_preserved", "keyframe completion", "Ref <N>"]
            {
                assert!(!fix.contains(r2v_only), "f2v fix with r2v wording: {fix}");
            }
        }
        for fix in &r2v {
            assert!(!fix.contains("integrated_multimodal_description") && !fix.contains("Picture 2"), "{fix}");
        }
        assert!(f2v.iter().any(|f| f.starts_with("Make integrated_multimodal_description longer (about 250 words)")));
    }

    #[test]
    fn fixes_sections_pick_the_mode() {
        let text = "MY FIXES - mine\n\nShared before any heading.\n=== r2v only ===\nR one.\n- R two.\n\n\
                    ==== F2V (first frame) ====\nF one.\n==== r2v and f2v ====\nBoth again.\n==== notes ====\nNo mode.";
        assert_eq!(
            parse_fixes_by_mode(text),
            vec![
                (None, "Shared before any heading.".to_string()),
                (Some(Mode::R2v), "R one.".to_string()),
                (Some(Mode::R2v), "R two.".to_string()),
                (Some(Mode::F2v), "F one.".to_string()),
                (None, "Both again.".to_string()),
                (None, "No mode.".to_string()),
            ]
        );
        let mut lib = Library::builtin();
        lib.texts.insert(LibraryItem::Fixes, text.to_string());
        assert_eq!(lib.fixes().len(), 6);
        assert_eq!(lib.fixes_for(Mode::F2v), vec!["Shared before any heading.", "F one.", "Both again.", "No mode."]);
        assert_eq!(lib.fixes_for(Mode::R2v).len(), 5);
        // Without headings every fix is for both modes.
        lib.texts.insert(LibraryItem::Fixes, "Make it rain.\nAdd a red umbrella.\n".to_string());
        assert_eq!(lib.fixes_for(Mode::F2v), lib.fixes());
    }

    #[test]
    fn fixes_parse_user_variants() {
        // No header, list markers, comments and CRLF.
        let text = "- Make it longer.\r\n# my own note\r\n* Remove the music.\r\n2. Add a smile.\r\n";
        assert_eq!(parse_fixes(text), vec!["Make it longer.", "Remove the music.", "Add a smile."]);
        // Header line directly above the fixes (one paragraph): only the title is dropped.
        assert_eq!(
            parse_fixes("MY FIXES\nMake it longer.\nUse a film look."),
            vec!["Make it longer.", "Use a film look."]
        );
        // A last paragraph that is a fix, not a note, is kept.
        assert_eq!(parse_fixes("Make it longer.\n\nUse a film look."), vec!["Make it longer.", "Use a film look."]);
        // Fixes that start with an abbreviation are not titles.
        assert_eq!(
            parse_fixes("I2VA line: keep it exact.\nOK, now make it longer."),
            vec!["I2VA line: keep it exact.", "OK, now make it longer."]
        );
        assert!(parse_fixes("").is_empty());
    }

    #[test]
    fn builtin_checklist_parse() {
        let items = Library::builtin().checklist_items();
        assert_eq!(items.len(), 16, "{items:#?}");
        let r2v_section = r"r2v prompt (workflow: 1 - Jiggle (use these)\minimaxh3_r2v jiggle (original layout).json)";
        assert_eq!(items[0].0, r2v_section);
        assert_eq!(
            items[0].1,
            "6 sections: subject_definitions, summary, retention_analysis, detailed_description, overall_soundscape, non_diegetic_music"
        );
        let mut sections: Vec<&str> = items.iter().map(|(s, _)| s.as_str()).collect();
        sections.dedup();
        assert_eq!(sections.len(), 3, "{sections:?}");
        assert_eq!(sections[2], "ComfyUI settings");
        assert_eq!(items.iter().filter(|(s, _)| s.starts_with("f2v prompt")).count(), 4);
        let line1 = items.iter().find(|(_, i)| i.starts_with("line 1 =")).map(|(_, i)| i.as_str());
        assert_eq!(
            line1,
            Some(
                "line 1 = \"For the target video, at 0.00 seconds ...\" (first frame only) or \"How the reference pictures \
                 align ...\" with the real length (first + last frame - turn the Last frame image ON)"
            )
        );
        assert!(items.iter().all(|(s, i)| !s.starts_with("CHECKLIST") && !i.starts_with("[ ]")));
    }

    #[test]
    fn checklist_parse_user_variants() {
        let text = "Before:\r\n- [ ] one\r\n  more of one\r\n[x] two\r\n\r\n   not a continuation\r\n[ ]\r\n    filled later\r\n[] three";
        assert_eq!(
            parse_checklist(text),
            vec![
                ("Before".to_string(), "one more of one".to_string()),
                ("Before".to_string(), "two".to_string()),
                ("not a continuation".to_string(), "filled later".to_string()),
                ("not a continuation".to_string(), "three".to_string()),
            ]
        );
    }

    #[test]
    fn overrides_load_set_and_reset_in_temp_dir() {
        let dir = temp_dir("overrides");
        std::fs::create_dir_all(&dir).unwrap();
        // A Notepad-style file: BOM + CRLF.
        std::fs::write(
            override_path_in(&dir, LibraryItem::Fixes),
            b"\xEF\xBB\xBFMY FIXES\r\n\r\nFix one.\r\nFix two.\r\n",
        )
        .unwrap();
        // Blank override files are ignored.
        std::fs::write(override_path_in(&dir, LibraryItem::Checklist), "  \r\n").unwrap();
        // An override equal to the built-in text (CRLF copy) is not an edit.
        let crlf_guide = normalize_text(LibraryItem::GuideF2v.default_text()).replace('\n', "\r\n");
        std::fs::write(override_path_in(&dir, LibraryItem::GuideF2v), crlf_guide).unwrap();

        let mut lib = Library::load_from(&dir);
        assert!(lib.is_overridden(LibraryItem::Fixes));
        assert_eq!(lib.text(LibraryItem::Fixes), "MY FIXES\n\nFix one.\nFix two.\n");
        assert_eq!(lib.fixes(), vec!["Fix one.", "Fix two."]);
        assert!(!lib.is_overridden(LibraryItem::Checklist));
        assert_eq!(lib.text(LibraryItem::Checklist), normalize_text(LibraryItem::Checklist.default_text()));
        assert!(!lib.is_overridden(LibraryItem::GuideF2v));
        assert!(!lib.is_overridden(LibraryItem::SystemR2v));

        // set_text writes the override (normalised) and a reload sees it.
        lib.set_text_in(&dir, LibraryItem::SystemR2v, "Custom system prompt.\r\nLine two.".to_string()).unwrap();
        assert!(lib.is_overridden(LibraryItem::SystemR2v));
        assert_eq!(lib.system_prompt(Mode::R2v), "Custom system prompt.\nLine two.");
        let on_disk = std::fs::read_to_string(override_path_in(&dir, LibraryItem::SystemR2v)).unwrap();
        assert_eq!(on_disk, "Custom system prompt.\nLine two.");
        let reloaded = Library::load_from(&dir);
        assert_eq!(reloaded.system_prompt(Mode::R2v), "Custom system prompt.\nLine two.");
        assert!(reloaded.is_overridden(LibraryItem::SystemR2v));

        // Saving the built-in text (even without its final newline) or a blank text resets.
        let builtin = normalize_text(LibraryItem::SystemR2v.default_text()).trim_end().to_string();
        lib.set_text_in(&dir, LibraryItem::SystemR2v, builtin).unwrap();
        assert!(!lib.is_overridden(LibraryItem::SystemR2v));
        assert!(!override_path_in(&dir, LibraryItem::SystemR2v).exists());
        lib.set_text_in(&dir, LibraryItem::Fixes, "   ".to_string()).unwrap();
        assert!(!lib.is_overridden(LibraryItem::Fixes));
        assert!(!override_path_in(&dir, LibraryItem::Fixes).exists());
        assert_eq!(lib.fixes().len(), 14);

        // reset deletes the file; resetting again (no file) is fine.
        lib.set_text_in(&dir, LibraryItem::Templates, "--- mine ---\nRef 1: me.".to_string()).unwrap();
        assert_eq!(lib.templates().len(), 1);
        lib.reset_in(&dir, LibraryItem::Templates).unwrap();
        assert!(!override_path_in(&dir, LibraryItem::Templates).exists());
        assert_eq!(lib.templates().len(), 12);
        lib.reset_in(&dir, LibraryItem::Templates).unwrap();
        assert_eq!(Library::load_from(&dir).text(LibraryItem::Templates), lib.text(LibraryItem::Templates));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_gives_builtins_and_utf16_overrides_decode() {
        let dir = temp_dir("utf16");
        assert_eq!(Library::load_from(&dir), Library::builtin());
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "Fix \u{2014} one.\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(override_path_in(&dir, LibraryItem::Fixes), bytes).unwrap();
        let lib = Library::load_from(&dir);
        assert_eq!(lib.fixes(), vec!["Fix \u{2014} one."]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
