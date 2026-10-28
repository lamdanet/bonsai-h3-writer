//! Every written prompt is saved as `config_dir()\history\<id>.json` (newest first in lists).
//!
//! Ids are sortable local times (`20261028-102644-123`), unique within the process: a clash (same
//! millisecond, or a clock that went back) gets a `-2`, `-3` ... suffix. Files are written
//! atomically (temp file + rename). The `*_in` functions take the history folder explicitly
//! (tests, portable use); the plain ones use [`history_dir`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context};
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::client::Timings;
use crate::gen::{GenParams, Mode};
use crate::lint::LintContext;
use crate::request::RequestSpec;

/// chrono format of the time part of history ids (`20261028-102644-123`).
pub const ID_FORMAT: &str = "%Y%m%d-%H%M%S-%3f";
/// Longest title (in characters, before `...`) made from a request.
pub const TITLE_MAX_CHARS: usize = 60;
/// Separates the original request from the fix in the `request` of a follow-up entry
/// (see [`HistoryEntry::original_request`] and [`split_fix`]).
pub const FIX_MARKER: &str = "\n\nFollow-up fix: ";

/// The last id handed out in this process: (time part, clash count).
static LAST_ID: Mutex<Option<(String, u32)>> = Mutex::new(None);

/// One saved prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryEntry {
    /// Sortable id from local time, e.g. `20261028-102644-123` (unique; append `-2`, `-3` on clashes).
    pub id: String,
    /// RFC 3339 local time.
    pub created: String,
    pub mode: Mode,
    /// Short title for lists.
    pub title: String,
    /// The request text sent to Bonsai.
    pub request: String,
    /// The cleaned H3 prompt.
    pub output: String,
    /// Bonsai's thinking text (empty with thinking off).
    pub reasoning: String,
    pub params: GenParams,
    pub timings: Option<Timings>,
    pub lint_score: Option<u8>,
    pub lint_errors: usize,
    pub duration_s: Option<u32>,
    /// Entry this one fixed (follow-up chain).
    pub parent_id: Option<String>,
    /// The structured form, when the GUI builder was used.
    pub spec: Option<RequestSpec>,
    /// The check facts this prompt was checked with (`None` in entries saved by older versions).
    pub lint_context: Option<LintContext>,
    /// Pictures sent with a typed request (a form keeps its own), so a follow-up can send them again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub image_paths: Vec<PathBuf>,
    pub starred: bool,
}

impl Default for HistoryEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            created: String::new(),
            mode: Mode::R2v,
            title: String::new(),
            request: String::new(),
            output: String::new(),
            reasoning: String::new(),
            params: GenParams::prompt_writer(),
            timings: None,
            lint_score: None,
            lint_errors: 0,
            duration_s: None,
            parent_id: None,
            spec: None,
            lint_context: None,
            image_paths: Vec::new(),
            starred: false,
        }
    }
}

impl HistoryEntry {
    /// New entry with a fresh id, `created` = now, title from the request's first ~60 chars.
    ///
    /// The title falls back to the output's first line, then to `Untitled`.
    pub fn new(mode: Mode, request: &str, output: &str) -> Self {
        let now = Local::now();
        let id = next_id(now.format(ID_FORMAT).to_string());
        let title = [request, output]
            .into_iter()
            .map(title_from)
            .find(|t| !t.is_empty())
            .unwrap_or_else(|| "Untitled".to_string());
        Self {
            id,
            created: now.to_rfc3339(),
            mode,
            title,
            request: request.to_string(),
            output: output.to_string(),
            ..Self::default()
        }
    }

    /// The request without the `Follow-up fix: ...` part of a follow-up entry (the text to put
    /// back into a request editor).
    pub fn original_request(&self) -> &str {
        split_fix(&self.request).0
    }

    /// The fix of a follow-up entry, `None` for a plain request.
    pub fn fix_text(&self) -> Option<&str> {
        split_fix(&self.request).1
    }

    /// `2026-10-28 10:26` from `created` (from the id when `created` cannot be parsed).
    pub fn short_date(&self) -> String {
        if let Ok(time) = chrono::DateTime::parse_from_rfc3339(&self.created) {
            return time.format("%Y-%m-%d %H:%M").to_string();
        }
        match self.id.get(..13) {
            Some(head) if is_date_minute(head) => {
                format!("{}-{}-{} {}:{}", &head[0..4], &head[4..6], &head[6..8], &head[9..11], &head[11..13])
            }
            _ => self.created.clone(),
        }
    }

    /// Case-insensitive search: every word of `query` occurs in the title, request, prompt,
    /// mode or id. An empty query matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if terms.is_empty() {
            return true;
        }
        let haystack =
            format!("{}\n{}\n{}\n{}\n{}", self.title, self.request, self.output, self.mode, self.id).to_lowercase();
        terms.iter().all(|term| haystack.contains(term.as_str()))
    }
}

/// Split a follow-up request into (original request, fix) at [`FIX_MARKER`].
pub fn split_fix(request: &str) -> (&str, Option<&str>) {
    match request.split_once(FIX_MARKER) {
        Some((original, fix)) => (original, Some(fix)),
        None => (request, None),
    }
}

/// First non-empty line of `text`, shortened to about [`TITLE_MAX_CHARS`] characters (cut at a
/// word when possible, `...` appended). Empty when `text` is blank.
pub fn title_from(text: &str) -> String {
    match text.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => shorten(line, TITLE_MAX_CHARS),
        None => String::new(),
    }
}

/// `line` cut to `max` characters on a char boundary (at a space in the last third when there is
/// one) with `...` appended.
fn shorten(line: &str, max: usize) -> String {
    let Some((cut, _)) = line.char_indices().nth(max) else {
        return line.to_string();
    };
    let head = &line[..cut];
    let ends_at_word = line[cut..].starts_with(char::is_whitespace);
    let min_keep = head.char_indices().nth(max * 2 / 3).map_or(0, |(i, _)| i);
    let head = match head.rfind(char::is_whitespace) {
        Some(pos) if !ends_at_word && pos >= min_keep => &head[..pos],
        _ => head,
    };
    let head = head.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-' | '.'));
    format!("{head}...")
}

/// `time_part`, unless it is not newer than the last id handed out in this process.
fn next_id(time_part: String) -> String {
    let mut last = match LAST_ID.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    unique_id(&mut last, time_part)
}

/// `time_part` when it is newer than the last one; otherwise `<last time part>-N` (same
/// millisecond, or the clock went back), so ids stay unique and increasing.
fn unique_id(last: &mut Option<(String, u32)>, time_part: String) -> String {
    if let Some((prev, count)) = last.as_mut() {
        if time_part.as_str() <= prev.as_str() {
            *count += 1;
            return format!("{prev}-{count}");
        }
    }
    *last = Some((time_part.clone(), 1));
    time_part
}

/// `YYYYMMDD-HHMMSS-mmm`.
fn is_time_part(s: &str) -> bool {
    s.len() == 19 && s.bytes().enumerate().all(|(i, b)| if i == 8 || i == 15 { b == b'-' } else { b.is_ascii_digit() })
}

/// `YYYYMMDD-HHMM`.
fn is_date_minute(s: &str) -> bool {
    s.len() == 13 && s.bytes().enumerate().all(|(i, b)| if i == 8 { b == b'-' } else { b.is_ascii_digit() })
}

/// Sort key of an id: (time part, clash suffix), so `...-123-10` sorts after `...-123-9`.
fn id_key(id: &str) -> (&str, u32) {
    if let (Some(time), Some(rest)) = (id.get(..19), id.get(19..)) {
        if is_time_part(time) {
            if let Some(n) = rest.strip_prefix('-').and_then(|n| n.parse::<u32>().ok()) {
                return (time, n);
            }
        }
    }
    (id, 1)
}

/// Ids become file names: refuse empty ids, path separators and characters Windows forbids.
fn check_id(id: &str) -> anyhow::Result<()> {
    let bad = id.trim().is_empty()
        || id.starts_with('.')
        || id.len() > 200
        || id.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'));
    if bad {
        bail!("invalid history id '{id}'");
    }
    Ok(())
}

/// `config_dir()\history` (created on demand).
pub fn history_dir() -> PathBuf {
    let dir = crate::config::config_dir().join("history");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// File of an entry inside `dir`.
pub fn entry_path_in(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Write `<id>.json`; returns the path.
pub fn save(entry: &HistoryEntry) -> anyhow::Result<PathBuf> {
    save_in(&history_dir(), entry)
}

/// [`save`] into `dir` (created when missing). Overwrites an entry with the same id.
pub fn save_in(dir: &Path, entry: &HistoryEntry) -> anyhow::Result<PathBuf> {
    check_id(&entry.id)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = entry_path_in(dir, &entry.id);
    let json = serde_json::to_string_pretty(entry).context("serialising the history entry")?;
    write_atomic(&path, json.as_bytes())?;
    Ok(path)
}

/// All entries, newest first (unreadable files are skipped).
///
/// Reads and parses every file: call it when the list is needed (workspace opened, after a save or
/// delete) and keep the result, not once per frame.
pub fn list() -> Vec<HistoryEntry> {
    list_in(&history_dir())
}

/// [`list`] of `dir` (empty when it does not exist). Corrupt files and entries without request
/// and prompt are skipped; the file name is the id.
pub fn list_in(dir: &Path) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> =
        json_files(dir).into_iter().filter_map(|(id, path)| read_entry(&path, &id).ok()).filter(is_listed).collect();
    entries.sort_by(|a, b| id_key(&b.id).cmp(&id_key(&a.id)));
    entries
}

/// Load one entry by id.
pub fn load(id: &str) -> anyhow::Result<HistoryEntry> {
    load_in(&history_dir(), id)
}

/// [`load`] from `dir`.
pub fn load_in(dir: &Path, id: &str) -> anyhow::Result<HistoryEntry> {
    check_id(id)?;
    let path = entry_path_in(dir, id);
    if !path.is_file() {
        bail!("no history entry '{id}' in {}", dir.display());
    }
    read_entry(&path, id)
}

/// Newest entry.
pub fn latest() -> Option<HistoryEntry> {
    latest_in(&history_dir())
}

/// [`latest`] of `dir` (only parses files until one is readable).
pub fn latest_in(dir: &Path) -> Option<HistoryEntry> {
    let mut files = json_files(dir);
    files.sort_by(|a, b| id_key(&b.0).cmp(&id_key(&a.0)));
    files.into_iter().filter_map(|(id, path)| read_entry(&path, &id).ok()).find(is_listed)
}

/// Delete one entry (error when it does not exist).
pub fn delete(id: &str) -> anyhow::Result<()> {
    delete_in(&history_dir(), id)
}

/// [`delete`] in `dir`.
pub fn delete_in(dir: &Path, id: &str) -> anyhow::Result<()> {
    check_id(id)?;
    let path = entry_path_in(dir, id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("no history entry '{id}' in {}", dir.display()),
        Err(e) => Err(e).with_context(|| format!("deleting {}", path.display())),
    }
}

/// Star or unstar an entry; returns the updated entry.
pub fn set_starred(id: &str, starred: bool) -> anyhow::Result<HistoryEntry> {
    set_starred_in(&history_dir(), id, starred)
}

/// [`set_starred`] in `dir`.
pub fn set_starred_in(dir: &Path, id: &str, starred: bool) -> anyhow::Result<HistoryEntry> {
    let mut entry = load_in(dir, id)?;
    entry.starred = starred;
    save_in(dir, &entry)?;
    Ok(entry)
}

/// Save the prompt as a plain `.txt` (UTF-8 with BOM so Notepad shows the em dash correctly).
pub fn export_txt(entry: &HistoryEntry, path: &Path) -> anyhow::Result<()> {
    write_notepad_txt(&entry.output, path)
}

/// Write `text` as a Notepad-friendly file: UTF-8 with BOM and CRLF line endings. Missing parent
/// folders are created.
pub fn write_notepad_txt(text: &str, path: &Path) -> anyhow::Result<()> {
    let text = crate::library::normalize_text(text);
    let mut bytes = Vec::with_capacity(text.len() + text.matches('\n').count() + 3);
    bytes.extend_from_slice(b"\xEF\xBB\xBF");
    bytes.extend_from_slice(text.replace('\n', "\r\n").as_bytes());
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// Write a file through a temp file + rename, so readers never see half a file. Falls back to a
/// plain write when the rename fails (Windows refuses to replace a file another program has open).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
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

/// `(stem, path)` of the `*.json` files in `dir`.
fn json_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("json")) && path.is_file()
        })
        .filter_map(|path| {
            let stem = path.file_stem()?.to_str()?.to_string();
            Some((stem, path))
        })
        .collect()
}

/// Parse one entry file; the id is the file name (so load/delete always find the file).
fn read_entry(path: &Path, id: &str) -> anyhow::Result<HistoryEntry> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let mut entry: HistoryEntry =
        serde_json::from_slice(bytes).with_context(|| format!("parsing {}", path.display()))?;
    entry.id = id.to_string();
    Ok(entry)
}

/// Entries without request and prompt (e.g. a stray `{}` file) are not shown.
fn is_listed(entry: &HistoryEntry) -> bool {
    !(entry.request.trim().is_empty() && entry.output.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty temp folder for one test (never the real config dir).
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bh3-history-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn entry(id: &str, request: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.to_string(),
            created: "2026-10-28T10:26:44.123+05:45".to_string(),
            request: request.to_string(),
            output: format!("prompt for {request}"),
            ..HistoryEntry::default()
        }
    }

    #[test]
    fn new_entry_has_time_id_created_and_title() {
        let e = HistoryEntry::new(Mode::F2v, "\n  Ref 1: a woman in a white blouse.  \nmore", "subject_definitions:");
        let (time, _) = id_key(&e.id);
        assert!(is_time_part(time), "{}", e.id);
        assert!(chrono::DateTime::parse_from_rfc3339(&e.created).is_ok(), "{}", e.created);
        assert_eq!(e.mode, Mode::F2v);
        assert_eq!(e.title, "Ref 1: a woman in a white blouse.");
        assert_eq!(e.request, "\n  Ref 1: a woman in a white blouse.  \nmore");
        assert_eq!(e.output, "subject_definitions:");
        assert_eq!(e.params, GenParams::prompt_writer());
        assert!(!e.starred && e.parent_id.is_none() && e.lint_score.is_none());

        assert_eq!(HistoryEntry::new(Mode::R2v, "  \n", "First prompt line\nsecond").title, "First prompt line");
        assert_eq!(HistoryEntry::new(Mode::R2v, "", "").title, "Untitled");
    }

    #[test]
    fn titles_are_shortened_on_char_boundaries() {
        let long = "Ref 1: a young woman with long dark-brown hair in a low ponytail, white blouse, black skirt";
        let title = title_from(long);
        assert!(title.ends_with("..."), "{title}");
        assert!(title.chars().count() <= TITLE_MAX_CHARS + 3, "{title}");
        assert_eq!(title, "Ref 1: a young woman with long dark-brown hair in a low...");
        // The cut falls exactly at the end of a word: keep that word.
        let at_word_end = format!("{}abcdef end", "abcde ".repeat(9));
        assert_eq!(title_from(&at_word_end), format!("{}abcdef...", "abcde ".repeat(9)));

        // Multi-byte characters and no spaces: cut exactly at 60 chars.
        let dashes = "\u{2014}".repeat(100);
        let title = title_from(&dashes);
        assert_eq!(title, format!("{}...", "\u{2014}".repeat(60)));
        assert_eq!(title_from("short"), "short");
        assert_eq!(title_from(&"x".repeat(60)), "x".repeat(60));
        assert_eq!(title_from(" \n\t"), "");
    }

    #[test]
    fn unique_ids_get_suffixes_and_keep_increasing() {
        let mut last = None;
        assert_eq!(unique_id(&mut last, "20261028-102644-123".into()), "20261028-102644-123");
        assert_eq!(unique_id(&mut last, "20261028-102644-123".into()), "20261028-102644-123-2");
        assert_eq!(unique_id(&mut last, "20261028-102644-123".into()), "20261028-102644-123-3");
        // The clock went back: still unique and newer.
        assert_eq!(unique_id(&mut last, "20261028-092644-000".into()), "20261028-102644-123-4");
        assert_eq!(unique_id(&mut last, "20261028-102644-124".into()), "20261028-102644-124");

        let mut ids: Vec<String> = (0..200).map(|_| HistoryEntry::new(Mode::R2v, "r", "o").id).collect();
        let created_order = ids.clone();
        ids.sort_by(|a, b| id_key(a).cmp(&id_key(b)));
        assert_eq!(ids, created_order, "ids sort in creation order");
        ids.dedup();
        assert_eq!(ids.len(), 200, "ids are unique");
    }

    #[test]
    fn id_keys_sort_numeric_suffixes() {
        let mut ids = vec![
            "20261028-102644-123-10",
            "20261028-102644-123",
            "20261028-102644-123-9",
            "20261028-102644-124",
            "20261028-102644-123-2",
        ];
        ids.sort_by(|a, b| id_key(b).cmp(&id_key(a)));
        assert_eq!(
            ids,
            vec![
                "20261028-102644-124",
                "20261028-102644-123-10",
                "20261028-102644-123-9",
                "20261028-102644-123-2",
                "20261028-102644-123",
            ]
        );
        assert_eq!(id_key("my-prompt"), ("my-prompt", 1));
        assert_eq!(
            id_key("\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}x"),
            ("\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}x", 1)
        );
    }

    #[test]
    fn save_load_round_trip_in_temp_dir() {
        let dir = temp_dir("roundtrip");
        let mut e = HistoryEntry::new(Mode::R2v, "Ref 1: a woman.", "subject_definitions:\n<Subject 1> is ...");
        e.reasoning = "thinking".into();
        e.timings = Some(Timings {
            prompt_n: 42,
            cache_n: 1500,
            prompt_ms: 812.5,
            prompt_per_second: 51.7,
            predicted_n: 640,
            predicted_ms: 15000.0,
            predicted_per_second: 42.7,
        });
        e.lint_score = Some(92);
        e.lint_errors = 0;
        e.duration_s = Some(6);
        e.parent_id = Some("20261028-102600-001".into());
        e.spec = Some(RequestSpec::default());
        e.starred = true;
        let path = save_in(&dir, &e).unwrap();
        assert_eq!(path, dir.join(format!("{}.json", e.id)));
        assert!(!dir.join(format!("{}.json.tmp", e.id)).exists());
        assert_eq!(load_in(&dir, &e.id).unwrap(), e);

        // Saving again overwrites the same file; the pictures of a typed request are kept.
        let mut changed = e.clone();
        changed.starred = false;
        changed.image_paths = vec![PathBuf::from(r"C:\photos\ref1.jpg")];
        save_in(&dir, &changed).unwrap();
        assert_eq!(list_in(&dir), vec![changed.clone()]);
        let json = std::fs::read_to_string(dir.join(format!("{}.json", e.id))).unwrap();
        assert!(json.contains("image_paths") && !serde_json::to_string(&e).unwrap().contains("image_paths"));
        assert!(set_starred_in(&dir, &e.id, true).unwrap().starred);
        assert!(load_in(&dir, &e.id).unwrap().starred);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_is_newest_first_and_skips_bad_files() {
        let dir = temp_dir("list");
        for (id, request) in [
            ("20261028-102644-123", "first"),
            ("20261028-102644-123-2", "second"),
            ("20261029-080000-000", "third"),
            ("20261027-235959-999", "oldest"),
        ] {
            save_in(&dir, &entry(id, request)).unwrap();
        }
        std::fs::write(dir.join("20261030-000000-000.json"), "not json {").unwrap();
        std::fs::write(dir.join("20261030-000000-001.json"), "{}").unwrap();
        std::fs::write(dir.join("20261030-000000-002.json.tmp"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "hello").unwrap();
        // A copied file: its name is the id, whatever the JSON says.
        let mut copied = entry("something-else", "copied");
        copied.id = "something-else".into();
        std::fs::write(dir.join("copy.json"), serde_json::to_string(&copied).unwrap()).unwrap();
        // Old or partial files load with defaults.
        std::fs::write(
            dir.join("20261026-120000-000.json"),
            "\u{feff}{\"id\":\"x\",\"request\":\"partial\",\"output\":\"o\",\"future_field\":1}",
        )
        .unwrap();

        let list = list_in(&dir);
        let ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "copy",
                "20261029-080000-000",
                "20261028-102644-123-2",
                "20261028-102644-123",
                "20261027-235959-999",
                "20261026-120000-000",
            ]
        );
        let partial = list.iter().find(|e| e.request == "partial").unwrap();
        assert_eq!(partial.params, GenParams::prompt_writer());
        assert_eq!(load_in(&dir, "copy").unwrap().request, "copied");

        assert_eq!(latest_in(&dir).map(|e| e.id), Some("copy".to_string()));
        assert!(list_in(&dir.join("missing")).is_empty());
        assert!(latest_in(&dir.join("missing")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_skips_unreadable_newest_files() {
        let dir = temp_dir("latest");
        save_in(&dir, &entry("20261028-102644-123", "good")).unwrap();
        std::fs::write(dir.join("20261029-000000-000.json"), "garbage").unwrap();
        assert_eq!(latest_in(&dir).map(|e| e.request), Some("good".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_and_invalid_ids() {
        let dir = temp_dir("delete");
        save_in(&dir, &entry("20261028-102644-123", "r")).unwrap();
        delete_in(&dir, "20261028-102644-123").unwrap();
        assert!(list_in(&dir).is_empty());
        assert!(delete_in(&dir, "20261028-102644-123").is_err());
        assert!(load_in(&dir, "20261028-102644-123").is_err());
        for bad in ["", "  ", "../escape", "a/b", r"a\b", "c:x", ".hidden", "x?"] {
            assert!(load_in(&dir, bad).is_err(), "{bad:?}");
            assert!(delete_in(&dir, bad).is_err(), "{bad:?}");
            assert!(save_in(&dir, &entry(bad, "r")).is_err(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_txt_is_utf8_bom_with_crlf() {
        let dir = temp_dir("export");
        let mut e = entry("20261028-102644-123", "r");
        e.output = "How the reference pictures align with the target video \u{2014} Picture 1.\n\nline\r\nlast".into();
        let path = dir.join("sub").join("prompt.txt");
        export_txt(&e, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"\xEF\xBB\xBF"));
        let text = std::str::from_utf8(&bytes[3..]).unwrap();
        assert_eq!(
            text,
            "How the reference pictures align with the target video \u{2014} Picture 1.\r\n\r\nline\r\nlast"
        );
        assert!(!text.replace("\r\n", "").contains('\n'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn followup_requests_split_and_search_and_dates() {
        let mut e = entry("20261028-102644-123", "Ref 1: a woman.");
        assert_eq!(e.original_request(), "Ref 1: a woman.");
        assert_eq!(e.fix_text(), None);
        e.request = format!("Ref 1: a woman.{FIX_MARKER}Make it longer.");
        assert_eq!(e.original_request(), "Ref 1: a woman.");
        assert_eq!(e.fix_text(), Some("Make it longer."));

        assert!(e.matches(""));
        assert!(e.matches("WOMAN longer"));
        assert!(e.matches("r2v"));
        assert!(!e.matches("woman cat"));

        assert_eq!(e.short_date(), "2026-10-28 10:26");
        e.created = "garbage".into();
        assert_eq!(e.short_date(), "2026-10-28 10:26");
        e.id = "copy".into();
        assert_eq!(e.short_date(), "garbage");
    }
}
