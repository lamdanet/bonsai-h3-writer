//! Turning a request into an H3 prompt with Bonsai: message building, output cleanup, lint, history.
//! Used by both the GUI service and the CLI.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::client::ChatResult;
use crate::gen::{ChatMessage, GenParams, ImageAttachment, Mode};
use crate::history::{self, HistoryEntry};
use crate::library::Library;
use crate::lint::{self, LintContext, LintReport};
use crate::request::{self, RequestSpec};

/// A follow-up fix on a previous answer ("Make the detailed_description longer ...").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowUp {
    pub previous_request: String,
    pub previous_output: String,
    pub fix: String,
    /// History id of the previous answer.
    pub parent_id: Option<String>,
}

/// One prompt-writing job.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteJob {
    pub mode: Mode,
    /// Plain-language request (from [`RequestSpec::build_text`] or typed by the user).
    pub request: String,
    pub params: GenParams,
    /// Pictures for Bonsai's vision input (usually empty: describing them in words is cheaper).
    pub images: Vec<ImageAttachment>,
    pub lint_ctx: LintContext,
    pub followup: Option<FollowUp>,
    /// The structured form, saved to history when present.
    pub spec: Option<RequestSpec>,
    /// Save to history when finished.
    pub save_history: bool,
}

impl WriteJob {
    /// Job from typed text with default lint context for `mode`.
    pub fn from_text(mode: Mode, request: impl Into<String>, params: GenParams) -> Self {
        Self {
            mode,
            request: request.into(),
            params,
            images: Vec::new(),
            lint_ctx: LintContext { mode, ..LintContext::default() },
            followup: None,
            spec: None,
            save_history: true,
        }
    }

    /// Job from the request builder.
    pub fn from_spec(spec: &RequestSpec, params: GenParams) -> anyhow::Result<Self> {
        Ok(Self {
            mode: spec.mode,
            request: spec.build_text(),
            params,
            images: spec.images()?,
            lint_ctx: spec.lint_context(),
            followup: None,
            spec: Some(spec.clone()),
            save_history: true,
        })
    }
}

impl WriteJob {
    /// Follow-up job that sends `fix` after a saved answer: same mode (so the system prompt and
    /// llama-server's cache stay the same), the entry's original request and prompt as the earlier
    /// turns, `parent_id` = the entry. The lint context is [`entry_lint_context`] (the facts of the
    /// latest answer, so a length or mode a fix changed sticks). The pictures of the first turn are
    /// attached again one by one (the form's, else [`HistoryEntry::image_paths`]); when some can no
    /// longer be read, the request's `Attached pictures ...` line lists only those that are sent.
    pub fn followup_of(entry: &HistoryEntry, fix: impl Into<String>, params: GenParams) -> Self {
        let fix = fix.into();
        let lint_ctx = entry_lint_context(entry);
        let mut previous_request = entry.original_request().to_string();
        let images = match &entry.spec {
            Some(spec) => {
                let (read, missing) = spec.readable_images();
                if !missing.is_empty() {
                    let names: Vec<String> = read.iter().map(|(name, _)| name.clone()).collect();
                    previous_request = request::with_attached_line(&previous_request, &names);
                }
                read.into_iter().map(|(_, image)| image).collect()
            }
            None => entry.image_paths.iter().filter_map(|path| ImageAttachment::from_file(path).ok()).collect(),
        };
        Self {
            mode: entry.mode,
            request: fix.clone(),
            params,
            images,
            lint_ctx,
            followup: Some(FollowUp {
                previous_request,
                previous_output: entry.output.clone(),
                fix,
                parent_id: Some(entry.id.clone()),
            }),
            spec: entry.spec.clone(),
            save_history: true,
        }
    }
}

/// Result of a finished job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriteOutcome {
    /// The cleaned H3 prompt.
    pub prompt: String,
    pub reasoning: String,
    pub lint: LintReport,
    pub result: ChatResult,
    /// History id when saved.
    pub history_id: Option<String>,
}

/// First user turn of a follow-up that has no earlier request (keeps user/assistant alternating).
const PLACEHOLDER_REQUEST: &str = "Write the prompt.";

/// Messages for Bonsai: system prompt of the mode, then either the request, or (follow-up)
/// request -> previous answer (assistant) -> fix. Images go on the request message.
/// Keeping the system prompt byte-identical between requests lets llama-server reuse its cache.
///
/// Follow-ups: the fix falls back to `job.request` when `fix` is empty, and the earlier request to
/// `job.request` when `previous_request` is empty; a `Follow-up fix:` part (history entries of
/// follow-ups, see [`history::FIX_MARKER`]) is cut off the earlier request.
pub fn build_messages(library: &Library, job: &WriteJob) -> Vec<ChatMessage> {
    let system = ChatMessage::system(library.system_prompt(job.mode));
    match &job.followup {
        None => vec![system, ChatMessage::user(job.request.as_str()).with_images(job.images.clone())],
        Some(followup) => {
            let (original, fix) = followup_parts(job, followup);
            let original = if original.trim().is_empty() { PLACEHOLDER_REQUEST } else { original };
            vec![
                system,
                ChatMessage::user(original).with_images(job.images.clone()),
                ChatMessage::assistant(followup.previous_output.as_str()),
                ChatMessage::user(fix),
            ]
        }
    }
}

/// (earlier request, fix) of a follow-up job, with the fallbacks described in [`build_messages`].
fn followup_parts<'a>(job: &'a WriteJob, followup: &'a FollowUp) -> (&'a str, &'a str) {
    let fix = if followup.fix.trim().is_empty() { job.request.as_str() } else { followup.fix.as_str() };
    let original = if !followup.previous_request.trim().is_empty() {
        followup.previous_request.as_str()
    } else if job.request.trim() != fix.trim() {
        job.request.as_str()
    } else {
        ""
    };
    (history::split_fix(original).0, fix)
}

/// Strip what is not part of the prompt: ``` fences, "Here is the prompt:" style lead-ins,
/// trailing notes after `non_diegetic_music: ...` line, surrounding whitespace. Converts markdown
/// section headings (`**subject_definitions**`, `### summary`) to `name:` form.
///
/// In detail:
/// - CRLF -> LF, a leaked `<think>...</think>` block is dropped, lines are trimmed.
/// - Fence lines go (```` ```text ````), fences glued to text are cut off.
/// - Headings of the known fields in any markdown form (`**summary:**`, `## Detailed description`,
///   `__non_diegetic_music__`, `1. **Retention analysis**`) become `name:`. The r2v block fields
///   (`subject_definitions`, `summary`, `retention_analysis`, `detailed_description`) get their
///   own line with the text below; the inline fields (`integrated_multimodal_description`,
///   `overall_soundscape`, `non_diegetic_music`) are `name: text` on one line.
/// - The prompt starts at the first `subject_definitions:` (r2v), the I2VA / FL2VA first line or
///   `integrated_multimodal_description:` (f2v), also when glued behind a lead-in on one line;
///   without those at the first field heading or prompt line (`<Subject N> ...`, `[Shot N] ...`);
///   without any of these nothing is cut.
/// - After the `non_diegetic_music` paragraph, chatter is dropped, unless more prompt sections or
///   shots follow (then nothing is cut).
/// - List markers and bold around `<Subject N>` / `<Picture N>` / `[Shot N]` go; a hyphen in the
///   FL2VA line becomes the em dash; one blank line between sections, none under a block heading.
///
/// A prompt that is already clean comes back unchanged.
pub fn clean_output(raw: &str) -> String {
    let text = crate::library::normalize_text(raw);
    let text = split_glued(after_think(&text));
    let raw_lines: Vec<Raw> = text.lines().map(classify).collect();
    let mut lines = restructure(&raw_lines);
    let start = find_start(&mut lines);
    let end = find_end(&lines, start).max(start);
    layout(&lines[start..end])
}

/// Clean + lint the answer and save history (when `job.save_history`).
///
/// The lint context is [`lint_context_for`]. Cancelled and empty answers are not saved; a failed
/// save leaves `history_id` empty and the outcome is returned anyway.
pub fn finish(job: &WriteJob, result: ChatResult) -> WriteOutcome {
    finish_with(job, result, &LINTER, history::save)
}

/// [`finish`] that saves into `history_dir` instead of the configured history folder.
pub fn finish_in(job: &WriteJob, result: ChatResult, history_dir: &Path) -> WriteOutcome {
    finish_with(job, result, &LINTER, |entry| history::save_in(history_dir, entry))
}

/// The prompt checks [`finish`] runs (a stand-in can be given in tests).
struct Linter {
    lint: fn(&str, &LintContext) -> LintReport,
    detect_mode: fn(&str) -> Option<Mode>,
}

/// The real checks from [`crate::lint`].
const LINTER: Linter = Linter { lint: lint::lint, detect_mode: lint::detect_mode };

fn finish_with(
    job: &WriteJob,
    result: ChatResult,
    linter: &Linter,
    save: impl FnOnce(&HistoryEntry) -> anyhow::Result<PathBuf>,
) -> WriteOutcome {
    let prompt = clean_output(&result.content);
    let ctx = context_with(job, &prompt, linter.detect_mode);
    let report = (linter.lint)(&prompt, &ctx);
    let reasoning = result.reasoning.trim().to_string();
    let history_id = if job.save_history && !result.cancelled && !prompt.is_empty() {
        let entry = build_history_entry(job, &result, &prompt, &ctx, &report);
        save(&entry).ok().map(|_| entry.id)
    } else {
        None
    };
    WriteOutcome { prompt, reasoning, lint: report, result, history_id }
}

/// The check facts of a saved entry, for checking it again or following it up: the facts its own
/// check used (entries saved by this version), else its form's facts when the form has the entry's
/// mode, else only the mode; a saved length wins over the form's (a fix can change the length).
pub fn entry_lint_context(entry: &HistoryEntry) -> LintContext {
    if let Some(ctx) = entry.lint_context.as_ref().filter(|c| c.mode == entry.mode) {
        return ctx.clone();
    }
    let mut ctx = match &entry.spec {
        Some(spec) if spec.mode == entry.mode => spec.lint_context(),
        _ => LintContext { mode: entry.mode, ..LintContext::default() },
    };
    if let Some(d) = entry.duration_s.filter(|d| *d > 0) {
        ctx.duration_s = Some(d as f32);
    }
    ctx
}

/// Lint context for a finished job: `job.lint_ctx` with the job's mode. For a follow-up, a new
/// length the fix asks for (`Make it 10 s ...`, see [`duration_in_fix`]; not a length it only
/// quotes from a check message) replaces the duration, and an answer that clearly is the
/// other prompt kind (`Rewrite it as an f2v ... prompt`) is linted as that kind (the picture facts
/// of the old kind are dropped).
pub fn lint_context_for(job: &WriteJob, prompt: &str) -> LintContext {
    context_with(job, prompt, lint::detect_mode)
}

fn context_with(job: &WriteJob, prompt: &str, detect_mode: fn(&str) -> Option<Mode>) -> LintContext {
    let mut ctx = LintContext { mode: job.mode, ..job.lint_ctx.clone() };
    if let Some(followup) = &job.followup {
        let (_, fix) = followup_parts(job, followup);
        if let Some(seconds) = duration_in_fix(fix) {
            ctx.duration_s = Some(seconds);
        }
        if let Some(mode) = detect_mode(prompt) {
            if mode != job.mode {
                ctx = LintContext { mode, duration_s: ctx.duration_s, ..LintContext::default() };
            }
        }
    }
    ctx
}

/// The history entry [`finish`] saves: request (for a follow-up: earlier request +
/// [`history::FIX_MARKER`] + fix), cleaned prompt, reasoning, settings, timings, lint result,
/// rounded duration, parent id and form. Title: `Fix: ...` for a follow-up, else the form's title,
/// else the request's first line.
pub fn build_history_entry(
    job: &WriteJob,
    result: &ChatResult,
    prompt: &str,
    ctx: &LintContext,
    report: &LintReport,
) -> HistoryEntry {
    let (request, title) = match &job.followup {
        Some(followup) => {
            let (original, fix) = followup_parts(job, followup);
            let request = if original.trim().is_empty() {
                fix.to_string()
            } else {
                format!("{original}{}{fix}", history::FIX_MARKER)
            };
            (request, Some(history::title_from(&format!("Fix: {}", fix.trim()))))
        }
        None => (job.request.clone(), job.spec.as_ref().map(|spec| history::title_from(&spec.title()))),
    };
    let mut entry = HistoryEntry::new(ctx.mode, &request, prompt);
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        entry.title = title;
    }
    entry.reasoning = result.reasoning.trim().to_string();
    entry.params = job.params.clone();
    entry.timings = result.timings.clone();
    entry.lint_score = Some(report.score);
    entry.lint_errors = report.errors();
    entry.duration_s = ctx.duration_s.filter(|d| d.is_finite() && *d > 0.0).map(|d| d.round() as u32);
    entry.parent_id = job.followup.as_ref().and_then(|f| f.parent_id.clone());
    entry.spec = job.spec.clone();
    if job.spec.is_none() {
        entry.image_paths = job.images.iter().filter_map(|image| image.path.clone()).collect();
    }
    entry.lint_context = Some(ctx.clone());
    entry
}

/// A new clip length a fix asks for: `Make it 10 s ...`, `Make it <10> s`, `change it to 8 seconds`,
/// `make the clip a 10-second one`, `It should be 8 s long`, `a 10-second version`, `Length: 12 s`.
/// Lengths a fix only quotes (`a 6 s clip is 6.58 s long` from a check message) do not count.
fn duration_in_fix(fix: &str) -> Option<f32> {
    let caps = duration_re()?.captures(fix)?;
    let value: f32 = caps.iter().skip(1).flatten().next()?.as_str().parse().ok()?;
    request::PLAUSIBLE_SECONDS.contains(&value).then_some(value)
}

// ---------------------------------------------------------------------------------------------
// Output cleanup internals
// ---------------------------------------------------------------------------------------------

/// The fields of an H3 prompt (r2v: the six from `subject_definitions`; f2v: the last three).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    SubjectDefinitions,
    Summary,
    RetentionAnalysis,
    DetailedDescription,
    IntegratedMultimodalDescription,
    OverallSoundscape,
    NonDiegeticMusic,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::SubjectDefinitions => "subject_definitions",
            Field::Summary => "summary",
            Field::RetentionAnalysis => "retention_analysis",
            Field::DetailedDescription => "detailed_description",
            Field::IntegratedMultimodalDescription => "integrated_multimodal_description",
            Field::OverallSoundscape => "overall_soundscape",
            Field::NonDiegeticMusic => "non_diegetic_music",
        }
    }

    /// Written `name: text` on one line (the others put `name:` on its own line).
    fn inline(self) -> bool {
        matches!(self, Field::IntegratedMultimodalDescription | Field::OverallSoundscape | Field::NonDiegeticMusic)
    }

    /// From heading words in any case and spacing (`Detailed description`, `non-diegetic music`).
    fn from_words(words: &str) -> Option<Field> {
        let key: String = words.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
        Some(match key.as_str() {
            "subjectdefinitions" | "subjectdefinition" => Field::SubjectDefinitions,
            "summary" => Field::Summary,
            "retentionanalysis" => Field::RetentionAnalysis,
            "detaileddescription" => Field::DetailedDescription,
            "integratedmultimodaldescription" => Field::IntegratedMultimodalDescription,
            "overallsoundscape" => Field::OverallSoundscape,
            "nondiegeticmusic" => Field::NonDiegeticMusic,
            _ => return None,
        })
    }
}

/// A line of the answer, classified.
#[derive(Debug, Clone, PartialEq)]
enum Raw {
    Blank,
    /// Markdown horizontal rule (`---`, `***`).
    Rule,
    /// A field heading with the text after it on the same line (may be empty).
    Heading(Field, String),
    Text(String),
}

/// A line in its cleaned form.
#[derive(Debug, Clone, PartialEq)]
enum Line {
    Blank,
    Rule,
    /// `name:` (bare) or `name: text`.
    Heading {
        field: Field,
        text: String,
        bare: bool,
    },
    Text(String),
}

const HEADING_PATTERN: &str = r"(?i)^(?:#{1,6}[ \t]*)?(?:(?:[-+*\x{2022}]|\d{1,2}[.)])[ \t]+)?(?P<open>[*_`]{0,3})[ \t]*(?P<name>subject[ \t_-]*definitions?|summary|retention[ \t_-]*analysis|detailed[ \t_-]*description|integrated[ \t_-]*multimodal[ \t_-]*description|overall[ \t_-]*soundscape|non[ \t_-]*diegetic[ \t_-]*music)(?:[ \t]*\([^)]*\))?[ \t]*(?P<c1>:?)[ \t]*(?P<close>[*_`]{0,3})[ \t]*(?P<c2>:?)[ \t]*(?P<rest>.*)$";
const LABEL_EMPHASIS_PATTERN: &str =
    r"(?:\*{1,2}|_{1,2})(?P<label>\[Shot \d+\]|<(?:Subject|Picture) \d+>)(?:\*{1,2}|_{1,2})";
/// Asking for a new length (see [`duration_in_fix`]): a verb + `it` / `the clip`, `be N s long`,
/// `a N-second version`, `length: N s`. One number group per alternative.
const DURATION_PATTERN: &str = concat!(
    r"(?i)\b(?:make|change|turn|set|extend|shorten|lengthen|stretch|trim|cut|rewrite|redo)\s+",
    r"(?:it|this|the\s+(?:clip|video|prompt|length|duration))\s+(?:(?:in)?to\s+|as\s+)?(?:an?\s+)?",
    r"<?(\d{1,2}(?:\.\d+)?)>?\s*-?\s*(?:s|secs?|seconds?)\b",
    r"|\bbe\s+<?(\d{1,2}(?:\.\d+)?)>?\s*-?\s*(?:s|secs?|seconds?)\s+long\b",
    r"|\ban?\s+<?(\d{1,2}(?:\.\d+)?)>?\s*-?\s*(?:s|secs?|seconds?)\s+version\b",
    r"|\b(?:new\s+)?(?:length|duration)\s*(?:to\s+|=\s*|:\s*)<?(\d{1,2}(?:\.\d+)?)>?\s*(?:s|secs?|seconds?)\b",
);

/// Compile a constant pattern once (`None` only if it were invalid; a unit test checks them).
fn cached_regex(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

fn heading_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, HEADING_PATTERN)
}

fn label_emphasis_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, LABEL_EMPHASIS_PATTERN)
}

fn duration_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, DURATION_PATTERN)
}

/// Start of the I2VA first line.
const I2VA_START: &str = "For the target video, at ";
/// Start of the FL2VA first line.
const FL2VA_START: &str = "How the reference pictures align";
/// The FL2VA line up to its em dash.
const FL2VA_HEAD: &str = "How the reference pictures align with the target video";

/// Lines that are prompt content even without a heading above them.
const PROMPT_LINE_STARTS: [&str; 5] =
    ["<Subject", "<Picture", "[Shot", "[reference generation", "[keyframe completion"];

/// The text after a leaked `</think>` (when something follows it).
fn after_think(text: &str) -> &str {
    const CLOSE: &str = "</think>";
    match text.rfind(CLOSE) {
        Some(pos) if !text[pos + CLOSE.len()..].trim().is_empty() => &text[pos + CLOSE.len()..],
        _ => text,
    }
}

/// Put a code fence or a first-section marker that is glued behind other text on its own line
/// (``Here is the prompt: ```subject_definitions:`` -> `Here is the prompt:` +
/// ```` ```subject_definitions: ````), so the lead-in can be dropped without the section.
fn split_glued(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut rest = line;
        while let Some(pos) = glued_split_point(rest) {
            out.push_str(&rest[..pos]);
            out.push('\n');
            rest = &rest[pos..];
        }
        out.push_str(rest);
    }
    out
}

/// Byte position of a fence / first-section marker that has real text before it on the line.
fn glued_split_point(line: &str) -> Option<usize> {
    const MARKERS: [&str; 4] = ["```", "~~~", "subject_definitions:", "integrated_multimodal_description:"];
    let mut best: Option<usize> = None;
    for marker in MARKERS {
        let fence = &marker[..1];
        let is_fence = fence == "`" || fence == "~";
        for (pos, _) in line.match_indices(marker) {
            let before = &line[..pos];
            if is_fence && before.ends_with(fence) {
                continue; // inside a longer fence run
            }
            let text_before =
                before.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '_' | '`' | '#' | '>' | '-'));
            if text_before.trim().is_empty() {
                continue;
            }
            best = Some(best.map_or(pos, |b| b.min(pos)));
            break;
        }
    }
    best
}

fn classify(line: &str) -> Raw {
    let line = strip_fences(line.trim());
    if line.is_empty() {
        return Raw::Blank;
    }
    if is_rule(line) {
        return Raw::Rule;
    }
    if let Some((field, rest)) = parse_heading(line) {
        return Raw::Heading(field, tidy_text(&rest));
    }
    Raw::Text(tidy_text(line))
}

/// Remove code-fence markers: a fence line (```` ``` ````, ```` ```text ````, `~~~`) becomes empty,
/// a fence glued to text (```` ```subject_definitions: ````, ```` N/A``` ````) is cut off.
fn strip_fences(line: &str) -> &str {
    let mut s = line;
    for (fence, mark) in [("```", '`'), ("~~~", '~')] {
        if let Some(rest) = s.strip_prefix(fence) {
            let rest = rest.trim_start_matches(mark).trim();
            let language_tag = rest.len() <= 20
                && rest.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '.' | '-'))
                && Field::from_words(rest).is_none();
            if language_tag {
                return "";
            }
            s = rest;
        }
        if let Some(rest) = s.strip_suffix(fence) {
            s = rest.trim_end_matches(mark).trim_end();
        }
    }
    s
}

/// `---`, `***`, `___`, `===`, `* * *`.
fn is_rule(line: &str) -> bool {
    let mut chars = line.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    let rest: Vec<char> = chars.collect();
    matches!(first, '-' | '*' | '_' | '=') && rest.len() >= 2 && rest.iter().all(|&c| c == first)
}

/// A field heading in markdown or plain form -> (field, text after it on the line).
fn parse_heading(line: &str) -> Option<(Field, String)> {
    let caps = heading_re()?.captures(line)?;
    let name = caps.name("name")?;
    // `summaryx`, `summary2` are not headings.
    if line[name.end()..].chars().next().is_some_and(char::is_alphanumeric) {
        return None;
    }
    let field = Field::from_words(name.as_str())?;
    let group = |n: &str| caps.name(n).map_or("", |m| m.as_str());
    let (open, close) = (group("open"), group("close"));
    let has_colon = !group("c1").is_empty() || !group("c2").is_empty();
    let mut rest = group("rest").trim();
    if !open.is_empty() && close.is_empty() {
        // The whole line is wrapped: `**summary: text**`.
        if let Some(inner) = rest.strip_suffix(open) {
            rest = inner.trim_end();
        }
    }
    let wrapped = !open.is_empty() && !close.is_empty();
    (wrapped || has_colon || rest.is_empty()).then(|| (field, rest.to_string()))
}

/// Light markdown cleanup of a content line: list markers before `<Subject N>` / `<Picture N>` /
/// `[Shot N]` and bold / italics around those labels go.
fn tidy_text(line: &str) -> String {
    let unlisted = strip_list_marker(line);
    let line = if starts_with_label(unlisted) { unlisted } else { line };
    match label_emphasis_re() {
        Some(re) => re.replace_all(line, "${label}").into_owned(),
        None => line.to_string(),
    }
}

fn starts_with_label(text: &str) -> bool {
    let text = text.trim_start_matches(['*', '_']);
    text.starts_with("<Subject") || text.starts_with("<Picture") || text.starts_with("[Shot")
}

/// Remove a list marker (`- `, `* `, `• `, `+ `, `1. `, `2) `).
fn strip_list_marker(line: &str) -> &str {
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

/// Headings into canonical form: block fields `name:` + text on the next line, inline fields
/// `name: text` (a bare inline heading takes the next paragraph line).
fn restructure(raw: &[Raw]) -> Vec<Line> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match &raw[i] {
            Raw::Blank => out.push(Line::Blank),
            Raw::Rule => out.push(Line::Rule),
            Raw::Text(text) => out.push(Line::Text(text.clone())),
            Raw::Heading(field, rest) => {
                let field = *field;
                let name = field.name();
                if !field.inline() {
                    out.push(Line::Heading { field, text: format!("{name}:"), bare: true });
                    if !rest.is_empty() {
                        out.push(Line::Text(rest.clone()));
                    }
                } else if !rest.is_empty() {
                    out.push(Line::Heading { field, text: format!("{name}: {rest}"), bare: false });
                } else {
                    let next = (i + 1..raw.len()).find(|&j| raw[j] != Raw::Blank);
                    match next.map(|j| (j, &raw[j])) {
                        Some((j, Raw::Text(text))) if joinable(text) => {
                            out.push(Line::Heading { field, text: format!("{name}: {text}"), bare: false });
                            i = j;
                        }
                        _ => out.push(Line::Heading { field, text: format!("{name}:"), bare: true }),
                    }
                }
            }
        }
        i += 1;
    }
    out
}

/// Text that may be pulled up behind a bare inline heading.
fn joinable(text: &str) -> bool {
    !is_chatter(text) && first_line_phrase(text).is_none()
}

/// Remarks to the user, not prompt text ("Let me know ...", "Note: ...").
fn is_chatter(text: &str) -> bool {
    const PREFIXES: [&str; 33] = [
        "note:",
        "notes:",
        "note -",
        "note that",
        "tip:",
        "tips:",
        "let me know",
        "i hope",
        "hope this",
        "feel free",
        "this prompt",
        "the prompt above",
        "the above prompt",
        "here is",
        "here's",
        "if you",
        "would you like",
        "do you want",
        "want me to",
        "shall i",
        "i can ",
        "i've ",
        "i have ",
        "i kept",
        "i used",
        "i made",
        "i added",
        "i changed",
        "i chose",
        "explanation",
        "changes:",
        "key changes",
        "enjoy",
    ];
    let lower = text
        .trim_start_matches(|c: char| matches!(c, '*' | '_' | '>' | '#' | '(' | '[') || c.is_whitespace())
        .to_lowercase();
    PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

/// Byte position of the I2VA / FL2VA first-line phrase in `text`.
fn first_line_phrase(text: &str) -> Option<usize> {
    [I2VA_START, FL2VA_START].iter().filter_map(|p| text.find(p)).min()
}

/// Index of the first prompt line; cuts a lead-in glued to the f2v first line.
fn find_start(lines: &mut [Line]) -> usize {
    for (i, line) in lines.iter_mut().enumerate() {
        match line {
            Line::Heading { field: Field::SubjectDefinitions | Field::IntegratedMultimodalDescription, .. } => {
                return i;
            }
            Line::Text(text) => {
                if let Some(pos) = first_line_phrase(text) {
                    if pos > 0 {
                        let wrapped = text[..pos].ends_with(['*', '_', '`']);
                        let mut cut = text[pos..].to_string();
                        if wrapped {
                            cut = cut.trim_end_matches(['*', '_', '`']).trim_end().to_string();
                        }
                        *text = cut;
                    }
                    return i;
                }
            }
            _ => {}
        }
    }
    // No first-section marker: start at the first heading or prompt line, so no prompt text is lost.
    lines
        .iter()
        .position(|line| match line {
            Line::Heading { .. } => true,
            Line::Text(text) => PROMPT_LINE_STARTS.iter().any(|p| text.starts_with(p)),
            _ => false,
        })
        .unwrap_or(0)
}

/// End (exclusive) of the prompt: after the `non_diegetic_music` paragraph, unless more prompt
/// sections or shots follow it.
fn find_end(lines: &[Line], start: usize) -> usize {
    let len = lines.len();
    let music = lines.iter().rposition(|line| matches!(line, Line::Heading { field: Field::NonDiegeticMusic, .. }));
    let Some(music) = music.filter(|&m| m >= start) else {
        return len;
    };
    let mut end = music + 1;
    while let Some(Line::Text(text)) = lines.get(end) {
        if is_chatter(text) {
            break;
        }
        end += 1;
    }
    let prompt_follows = lines[end..].iter().any(|line| match line {
        Line::Heading { .. } => true,
        Line::Text(text) => text.starts_with("[Shot"),
        _ => false,
    });
    if prompt_follows {
        len
    } else {
        end
    }
}

/// Join the lines: one blank line before each heading and between paragraphs, none right under a
/// block heading, rules dropped.
fn layout(lines: &[Line]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut pending_blank = false;
    let mut under_block_heading = false;
    for line in lines {
        match line {
            Line::Blank | Line::Rule => {
                if !under_block_heading {
                    pending_blank = true;
                }
            }
            Line::Heading { field, text, bare } => {
                if !out.is_empty() {
                    out.push(String::new());
                }
                out.push(text.clone());
                pending_blank = false;
                under_block_heading = *bare && !field.inline();
            }
            Line::Text(text) => {
                if pending_blank && !out.is_empty() {
                    out.push(String::new());
                }
                out.push(fix_fl2va_dash(text));
                pending_blank = false;
                under_block_heading = false;
            }
        }
    }
    out.join("\n")
}

/// `How the reference pictures align with the target video - Picture 1 ...` -> em dash.
fn fix_fl2va_dash(text: &str) -> String {
    if let Some(rest) = text.strip_prefix(FL2VA_HEAD) {
        let after = rest.trim_start();
        for sep in ["---", "--", "-", "\u{2013}", ":"] {
            if let Some(tail) = after.strip_prefix(sep) {
                return format!("{FL2VA_HEAD} \u{2014} {}", tail.trim_start());
            }
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Timings;
    use crate::gen::Role;

    const CLEAN_R2V: &str = "subject_definitions:
<Subject 1> is the woman in <Picture 1>, a young woman with long dark-brown hair in a low ponytail, wearing a crisp white blouse.
<Subject 2> is the cafe environment in <Picture 2>, with tall sunlit windows and oak tables.

summary:
[reference generation] The target video shows <Subject 1> sitting down at a window table in <Subject 2>, in one continuous take.

retention_analysis:
<Subject 1> (appears in [Shot 1]): fully_preserved - her face, ponytail and white blouse are retained.
<Subject 2> (appears in [Shot 1]): fully_preserved - the windows and oak tables are retained.

detailed_description:
The target video is a cinematic live-action clip with a 35mm look and warm morning light. No subtitles, captions, text, watermarks or logos.
[Shot 1] A medium shot frames <Subject 1>, the woman from <Picture 1>, at the counter of <Subject 2>; she walks to the window table and sits, and the camera tracks with her at slow speed.

overall_soundscape: Quiet cafe room tone with a faint espresso-machine hiss and the scrape of a chair.

non_diegetic_music: Soft acoustic guitar at a slow tempo with light, sparse notes.";

    const CLEAN_F2V: &str = "For the target video, at 0.00 seconds into the target video, <Picture 1> (from [Shot 1]) is fully referenced.

integrated_multimodal_description: [Shot 1] Live-action, cinematic, a medium shot opens on <Picture 1>: a young woman in a white blouse stands at a cafe counter, then walks to a window table and sits while the camera tracks with her at slow speed. No subtitles, captions, text, watermarks or logos.

overall_soundscape: Quiet cafe room tone with her heels on the floor.

non_diegetic_music: N/A";

    /// `clean_output`, checking that cleaning twice changes nothing.
    fn clean(raw: &str) -> String {
        let once = clean_output(raw);
        assert_eq!(clean_output(&once), once, "clean_output must be idempotent for:\n{raw}");
        once
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bh3-writer-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn chat_result(content: &str) -> ChatResult {
        ChatResult {
            content: content.to_string(),
            reasoning: "  thought  ".to_string(),
            finish_reason: "stop".to_string(),
            timings: Some(Timings {
                prompt_n: 120,
                cache_n: 1800,
                predicted_n: 640,
                predicted_per_second: 42.0,
                ..Timings::default()
            }),
            usage: None,
            elapsed_ms: 15_000,
            cancelled: false,
        }
    }

    #[test]
    fn patterns_compile() {
        assert!(heading_re().is_some());
        assert!(label_emphasis_re().is_some());
        assert!(duration_re().is_some());
    }

    #[test]
    fn messages_for_a_request() {
        let lib = Library::builtin();
        let img = ImageAttachment { path: None, mime: "image/png".into(), base64: "AAAA".into() };
        let mut job = WriteJob::from_text(Mode::R2v, "Ref 1: a woman.", GenParams::prompt_writer());
        job.images = vec![img.clone()];
        let msgs = build_messages(&lib, &job);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content, lib.system_prompt(Mode::R2v));
        assert!(msgs[0].images.is_empty());
        assert_eq!(msgs[1].role, Role::User);
        assert_eq!(msgs[1].content, "Ref 1: a woman.");
        assert_eq!(msgs[1].images, vec![img]);

        // The system prompt is byte-identical between jobs (llama-server prompt cache).
        let other = WriteJob::from_text(Mode::R2v, "Something else", GenParams::chat());
        assert_eq!(build_messages(&lib, &other)[0], msgs[0]);
        let f2v = WriteJob::from_text(Mode::F2v, "Picture 1: a man.", GenParams::prompt_writer());
        assert_eq!(build_messages(&lib, &f2v)[0].content, lib.system_prompt(Mode::F2v));
    }

    #[test]
    fn messages_for_a_followup() {
        let lib = Library::builtin();
        let img = ImageAttachment { path: None, mime: "image/jpeg".into(), base64: "BBBB".into() };
        let mut job = WriteJob::from_text(Mode::R2v, "Remove the music.", GenParams::prompt_writer());
        job.images = vec![img.clone()];
        job.followup = Some(FollowUp {
            previous_request: format!("Ref 1: a woman.{}Make it longer.", history::FIX_MARKER),
            previous_output: "old prompt".into(),
            fix: "Remove the music.".into(),
            parent_id: Some("20261028-102644-123".into()),
        });
        let msgs = build_messages(&lib, &job);
        let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::System, Role::User, Role::Assistant, Role::User]);
        assert_eq!(msgs[0].content, lib.system_prompt(Mode::R2v));
        assert_eq!(msgs[1].content, "Ref 1: a woman.");
        assert_eq!(msgs[1].images, vec![img]);
        assert_eq!(msgs[2].content, "old prompt");
        assert!(msgs[2].images.is_empty());
        assert_eq!(msgs[3].content, "Remove the music.");
        assert!(msgs[3].images.is_empty());

        // An empty fix falls back to the job's request.
        if let Some(f) = job.followup.as_mut() {
            f.fix.clear();
        }
        job.request = "Use a film look.".into();
        let msgs = build_messages(&lib, &job);
        assert_eq!(msgs[1].content, "Ref 1: a woman.");
        assert_eq!(msgs[3].content, "Use a film look.");

        // No earlier request at all: a placeholder keeps user/assistant alternating.
        if let Some(f) = job.followup.as_mut() {
            f.previous_request.clear();
        }
        let msgs = build_messages(&lib, &job);
        assert_eq!(msgs[1].content, PLACEHOLDER_REQUEST);
        assert_eq!(msgs[3].content, "Use a film look.");
    }

    #[test]
    fn clean_prompts_are_unchanged() {
        assert_eq!(clean(CLEAN_R2V), CLEAN_R2V);
        assert_eq!(clean(CLEAN_F2V), CLEAN_F2V);
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !name.starts_with("good_") {
                continue;
            }
            let text = crate::library::normalize_text(&std::fs::read_to_string(&path).unwrap());
            let expected = text.trim();
            assert_eq!(clean(&text), expected, "{name}");
            assert_eq!(clean(&text.replace('\n', "\r\n")), expected, "{name} (CRLF)");
            // The same answer wrapped the way chatty models write it cleans back to the original.
            let messy = format!(
                "Sure! Here is the prompt:\n\n```text\n{}\n```\n\nI hope this helps! Let me know if you want changes.",
                markdownify(expected)
            );
            assert_ne!(messy, expected);
            assert_eq!(clean(&messy), expected, "{name} (markdown)");
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} fixtures found in {}", dir.display());
    }

    /// Markdown headings instead of `name:` lines: `**summary**` + blank line for block fields,
    /// `### overall soundscape` + blank line + text for inline fields.
    fn markdownify(prompt: &str) -> String {
        let is_field =
            |name: &str| Field::from_words(name).is_some() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        let mut out = Vec::new();
        for line in prompt.lines() {
            if let Some(name) = line.strip_suffix(':').filter(|n| is_field(n)) {
                out.push(format!("**{name}**"));
                out.push(String::new());
            } else if let Some((name, rest)) = line.split_once(": ").filter(|(n, _)| is_field(n)) {
                out.push(format!("### {}", name.replace('_', " ")));
                out.push(String::new());
                out.push(rest.to_string());
            } else {
                out.push(line.to_string());
            }
        }
        out.join("\n")
    }

    #[test]
    fn cleaning_leaves_no_structural_lint_issues() {
        let raw = include_str!("../tests/fixtures/bad_r2v_markdown_fake_shots.txt");
        let ctx = LintContext {
            mode: Mode::R2v,
            duration_s: Some(15.0),
            ref_count: Some(2),
            first_frame_pinned: Some(false),
            has_last_frame: Some(false),
        };
        let codes = |text: &str| lint::lint(text, &ctx).issues.into_iter().map(|i| i.code).collect::<Vec<_>>();
        let before = codes(raw);
        assert!(before.iter().any(|c| c == "markdown-headings"), "{before:?}");
        let after = codes(&clean_output(raw));
        for structural in [
            "markdown-headings",
            "missing-section",
            "empty-section",
            "duplicate-section",
            "section-order",
            "text-before-sections",
            "text-after-sections",
        ] {
            assert!(!after.iter().any(|c| c == structural), "{structural} after cleaning: {after:?}");
        }
        // Content problems stay for the user (or a follow-up fix) to see.
        assert!(after.iter().any(|c| c == "shot-time-format"), "{after:?}");

        // f2v: lead-in, markdown and chatter around a good answer -> no structural issues.
        let f2v = include_str!("../tests/fixtures/good_f2v_e8.txt");
        let ctx = LintContext {
            mode: Mode::F2v,
            duration_s: Some(3.0),
            ref_count: Some(2),
            first_frame_pinned: Some(false),
            has_last_frame: Some(true),
        };
        let messy = format!("Here you go:\n\n{}\n\n---\nNote: one shot.", markdownify(f2v.trim()));
        let after: Vec<String> = lint::lint(&clean_output(&messy), &ctx).issues.into_iter().map(|i| i.code).collect();
        let clean_codes: Vec<String> = lint::lint(f2v, &ctx).issues.into_iter().map(|i| i.code).collect();
        assert_eq!(after, clean_codes);
    }

    #[test]
    fn strips_fences_lead_in_and_trailing_chatter() {
        let raw = format!(
            "Sure! Here is the prompt:\n\n```text\n{CLEAN_R2V}\n```\n\nLet me know if you want a longer description!\nHappy rendering."
        );
        assert_eq!(clean(&raw), CLEAN_R2V);
        let raw = format!("```\n{CLEAN_F2V}```");
        assert_eq!(clean(&raw), CLEAN_F2V);
        let raw = format!("Here is the prompt: ```{CLEAN_R2V}\n```");
        assert_eq!(clean(&raw), CLEAN_R2V);
    }

    #[test]
    fn glued_markers_and_missing_first_heading_keep_prompt_text() {
        assert_eq!(clean(&format!("Here is the prompt: {CLEAN_R2V}")), CLEAN_R2V);
        assert_eq!(clean(&format!("Here is the prompt: **{}", CLEAN_R2V.replacen(':', ":**", 1))), CLEAN_R2V);
        let imd_only = CLEAN_F2V.split_once("\n\n").map(|(_, rest)| rest).unwrap();
        assert_eq!(clean(&format!("Here you go: {imd_only}")), imd_only);

        // No subject_definitions heading: the subject lines are kept, the lead-in goes.
        let without_heading = CLEAN_R2V.replacen("subject_definitions:\n", "Sure, here it is.\n\n", 1);
        let cleaned = clean(&without_heading);
        assert!(cleaned.starts_with("<Subject 1> is the woman in <Picture 1>"), "{cleaned}");
        assert!(cleaned.ends_with("light, sparse notes."), "{cleaned}");
    }

    #[test]
    fn converts_markdown_headings() {
        let raw = CLEAN_R2V
            .replace("subject_definitions:\n", "### subject_definitions\n")
            .replace("summary:\n", "**summary:**\n\n")
            .replace("retention_analysis:\n", "## Retention analysis\n\n")
            .replace("detailed_description:\n", "## Detailed description\n")
            .replace("overall_soundscape: ", "**Overall soundscape:** ")
            .replace("non_diegetic_music: ", "**non_diegetic_music**\n\n");
        assert_eq!(raw.matches("**").count(), 6, "{raw}");
        assert_eq!(clean(&raw), CLEAN_R2V);

        let raw = CLEAN_R2V
            .replace("subject_definitions:\n", "1. __Subject Definitions__\n")
            .replace("summary:\n", "**Summary**:\n")
            .replace("retention_analysis:\n", "`retention_analysis`\n")
            .replace("detailed_description:\n", "**Detailed description (350-500 words):**\n\n")
            .replace("overall_soundscape: ", "### Overall soundscape\n")
            .replace("non_diegetic_music: ", "*Non-diegetic music:* ");
        assert_eq!(clean(&raw), CLEAN_R2V);
    }

    #[test]
    fn bad_fixture_headings_are_fixed_and_no_text_is_lost() {
        let raw = include_str!("../tests/fixtures/bad_r2v_markdown_fake_shots.txt");
        let cleaned = clean(raw);
        assert!(!cleaned.contains("**"), "{cleaned}");
        assert!(cleaned.starts_with("subject_definitions:\n<Subject 1> is the dark-haired woman"), "{cleaned}");
        for part in [
            "\n\nsummary:\n[reference generation] The target video",
            "\n\nretention_analysis:\n<Subject 1> (appears in",
            "\n\ndetailed_description:\nThe target video is a 15-second",
            "\n[Shot 2] Around 00:05.000 the camera",
            "\n\noverall_soundscape: Soft indoor room tone; no distinct sound events.",
        ] {
            assert!(cleaned.contains(part), "missing {part:?} in:\n{cleaned}");
        }
        assert!(cleaned.ends_with("\n\nnon_diegetic_music: N/A"), "{cleaned}");
        for line in
            crate::library::normalize_text(raw).lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("**"))
        {
            assert!(cleaned.contains(line), "lost {line:?}");
        }
    }

    #[test]
    fn f2v_lead_in_and_line_one_label() {
        let body = CLEAN_F2V
            .replace("integrated_multimodal_description: ", "**integrated_multimodal_description:**\n")
            .replace("overall_soundscape: ", "**overall_soundscape:** ")
            .replace("non_diegetic_music: ", "**non_diegetic_music:** ");
        let raw = format!("Sure! Here's your f2v prompt:\n\n**Line 1:** {body}\n\n---\n*Note: I kept one shot.*");
        assert_eq!(clean(&raw), CLEAN_F2V);
    }

    #[test]
    fn bold_first_line_and_fl2va_hyphen() {
        let fl2va = "How the reference pictures align with the target video \u{2014} Picture 1 (from Shot 1) aligns with \
                     the 0.00-second mark of the target video; Picture 2 (from Shot 1) aligns with the 6.58-second mark \
                     of the target video.";
        let rest =
            "integrated_multimodal_description: [Shot 1] Live-action, cinematic, she walks to the table, settling \
                    into the pose, spacing, light and composition established by Picture 2 at the end of the shot.\n\n\
                    overall_soundscape: Quiet cafe room tone.\n\nnon_diegetic_music: N/A";
        let expected = format!("{fl2va}\n\n{rest}");
        let raw = format!("**{}**\n\n{rest}", fl2va.replace(" \u{2014} ", " - "));
        assert_eq!(clean(&raw), expected);
        let raw = format!("Prompt: {}\n{rest}", fl2va.replace(" \u{2014} ", " -- "));
        assert_eq!(clean(&raw), expected);
        assert_eq!(clean(&expected), expected);
    }

    #[test]
    fn crlf_and_think_block() {
        let raw = format!(
            "<think>\nI should write the prompt.\nsubject_definitions: hmm\n</think>\n\n{}",
            CLEAN_R2V.replace('\n', "\r\n")
        );
        assert_eq!(clean(&raw), CLEAN_R2V);
        assert_eq!(clean(&format!("\u{feff}{CLEAN_F2V}\r\n")), CLEAN_F2V);
    }

    #[test]
    fn trailing_chatter_variants() {
        assert_eq!(clean(&format!("{CLEAN_R2V}\nLet me know if you want any changes.")), CLEAN_R2V);
        assert_eq!(clean(&format!("{CLEAN_R2V}\n---\nNotes: the outfit is kept.")), CLEAN_R2V);
        assert_eq!(clean(&format!("{CLEAN_R2V}\n\nThis keeps the outfit from Ref 1.\n\nEnjoy!")), CLEAN_R2V);
        assert_eq!(clean(&format!("{CLEAN_F2V}\n\n**Note:** one shot only.")), CLEAN_F2V);
    }

    #[test]
    fn nothing_is_cut_when_prompt_text_follows_the_music() {
        let raw = "subject_definitions:\n<Subject 1> is the woman in <Picture 1>.\n\nnon_diegetic_music: N/A\n\n\
                   summary:\n[reference generation] The target video shows <Subject 1> walking.";
        assert_eq!(clean(raw), raw);
        let raw = format!("{CLEAN_R2V}\n\n[Shot 2] At 00:03.000, the camera cuts to her smile.");
        assert_eq!(clean(&raw), raw);
    }

    #[test]
    fn text_without_sections_is_kept() {
        assert_eq!(
            clean("  Here is a free-form idea.\n\n\n\nShe walks in the rain.  "),
            "Here is a free-form idea.\n\nShe walks in the rain."
        );
        assert_eq!(clean(""), "");
        assert_eq!(clean(" \n```\n```\n"), "");
    }

    #[test]
    fn missing_blank_lines_and_inline_block_fields() {
        let squashed = "subject_definitions: <Subject 1> is the woman in <Picture 1>.\n\
                        summary: [reference generation] The target video shows <Subject 1> walking.\n\
                        retention_analysis:\n\n<Subject 1> (appears in [Shot 1]): fully_preserved - her face is retained.\n\
                        detailed_description:\nThe target video is a film scene.\n[Shot 1] She walks.\n\
                        overall_soundscape:\nRain on the pavement.\nnon_diegetic_music:\n\nN/A";
        let expected = "subject_definitions:\n<Subject 1> is the woman in <Picture 1>.\n\n\
                        summary:\n[reference generation] The target video shows <Subject 1> walking.\n\n\
                        retention_analysis:\n<Subject 1> (appears in [Shot 1]): fully_preserved - her face is retained.\n\n\
                        detailed_description:\nThe target video is a film scene.\n[Shot 1] She walks.\n\n\
                        overall_soundscape: Rain on the pavement.\n\nnon_diegetic_music: N/A";
        assert_eq!(clean(squashed), expected);
    }

    #[test]
    fn labels_lose_bullets_and_emphasis() {
        let raw = "subject_definitions:\n- **<Subject 1>** is the woman in <Picture 1>.\n* <Subject 2> is the cafe in <Picture 2>.\n\n\
                   detailed_description:\nThe target video is a film scene.\n**[Shot 1]** She walks.\n\
                   2. [Shot 2] At 00:03.000, the camera cuts to her smile.\n- a plain bullet stays";
        let expected = "subject_definitions:\n<Subject 1> is the woman in <Picture 1>.\n<Subject 2> is the cafe in <Picture 2>.\n\n\
                        detailed_description:\nThe target video is a film scene.\n[Shot 1] She walks.\n\
                        [Shot 2] At 00:03.000, the camera cuts to her smile.\n- a plain bullet stays";
        assert_eq!(clean(raw), expected);
    }

    #[test]
    fn heading_variants() {
        use Field::*;
        for (line, field, rest) in [
            ("subject_definitions:", SubjectDefinitions, ""),
            ("**subject_definitions**", SubjectDefinitions, ""),
            ("**summary:**", Summary, ""),
            ("**summary**:", Summary, ""),
            ("### summary", Summary, ""),
            ("#summary:", Summary, ""),
            ("Summary", Summary, ""),
            ("## Detailed description", DetailedDescription, ""),
            ("**Detailed Description (350-500 words):**", DetailedDescription, ""),
            ("__non_diegetic_music__", NonDiegeticMusic, ""),
            ("*Non-diegetic music:* N/A", NonDiegeticMusic, "N/A"),
            ("`overall_soundscape:` Rain.", OverallSoundscape, "Rain."),
            ("1. **Retention Analysis**", RetentionAnalysis, ""),
            (
                "- integrated_multimodal_description: [Shot 1] Live-action",
                IntegratedMultimodalDescription,
                "[Shot 1] Live-action",
            ),
            ("**summary: [reference generation] text**", Summary, "[reference generation] text"),
            ("**summary** [reference generation] text", Summary, "[reference generation] text"),
            ("overall_soundscape:Rain.", OverallSoundscape, "Rain."),
        ] {
            assert_eq!(parse_heading(line), Some((field, rest.to_string())), "{line}");
        }
        for line in [
            "Summary of the scene",
            "**Summary of the scene**",
            "### Summary of the scene",
            "summary_of_x: y",
            "summaryx:",
            "<Subject 1> is the woman",
            "The summary: x",
            "Overall, the soundscape is quiet.",
            "[reference generation] The target video",
        ] {
            assert_eq!(parse_heading(line), None, "{line}");
        }
    }

    #[test]
    fn durations_named_in_fixes() {
        assert_eq!(duration_in_fix("Make it 10 s with 2 cuts: [Shot 2] At 00:0X.000 ..."), Some(10.0));
        assert_eq!(duration_in_fix("Make it <10> s with <2> cuts"), Some(10.0));
        assert_eq!(duration_in_fix("make it 15 seconds"), Some(15.0));
        assert_eq!(duration_in_fix("Make it 4.5s, one take"), Some(4.5));
        assert_eq!(duration_in_fix("It should be 8 s long."), Some(8.0));
        assert_eq!(duration_in_fix("Make the detailed_description longer (about 350 words)"), None);
        assert_eq!(duration_in_fix("Add a 2 s pause before she speaks."), None);
        assert_eq!(duration_in_fix("Make it a close side view from the bust up."), None);
        assert_eq!(duration_in_fix("Write a 10-second version with 2 cuts."), Some(10.0));
        assert_eq!(duration_in_fix("Change it to 8 seconds."), Some(8.0));
        assert_eq!(duration_in_fix("Make the clip a 12-second one."), Some(12.0));
        assert_eq!(duration_in_fix("New length: 20 s."), Some(20.0));
        // Lengths quoted from check messages (`bh3 fix --from-lint`) are not new lengths.
        for quoted in [
            "Fix these problems and keep everything else the same:\n- [Shot 3] at 00:07.000 is past the end: a 6 s clip is 6.58 s long",
            "- line 1 says Picture 2 aligns with the 6.00-second mark, but a 6 s clip is really 6.58 s long: write \"the 6.58-second mark\"",
            "- 5 cuts in a 15 s clip; about one cut per 3 s (up to 4) works best",
            "Make it 45 s long.",
        ] {
            assert_eq!(duration_in_fix(quoted), None, "{quoted}");
        }
    }

    #[test]
    fn fix_chain_keeps_the_latest_length_and_mode() {
        let dir = temp_dir("fix-chain");
        let save = |e: &HistoryEntry| history::save_in(&dir, e);
        let spec = RequestSpec {
            refs: vec![
                crate::request::RefImage { description: "a woman".into(), ..Default::default() },
                crate::request::RefImage {
                    role: crate::request::RefRole::Place,
                    description: "a cafe".into(),
                    ..Default::default()
                },
            ],
            action: "she sits down".into(),
            ..RequestSpec::default()
        };
        let first = finish_with(
            &WriteJob::from_spec(&spec, GenParams::prompt_writer()).unwrap(),
            chat_result(CLEAN_R2V),
            &FAKE_LINTER,
            save,
        );
        let a = history::load_in(&dir, first.history_id.as_deref().unwrap()).unwrap();
        assert_eq!(entry_lint_context(&a).duration_s, Some(6.0));

        // A fix that changes the length: later fixes are checked against it, also when they quote
        // a check message with the old numbers.
        let job = WriteJob::followup_of(&a, "Make it 10 s with 2 cuts.", GenParams::prompt_writer());
        let b = finish_with(&job, chat_result(CLEAN_R2V), &FAKE_LINTER, save);
        let b = history::load_in(&dir, b.history_id.as_deref().unwrap()).unwrap();
        assert_eq!((b.duration_s, entry_lint_context(&b).duration_s), (Some(10), Some(10.0)));
        let quoting = "Fix these problems and keep everything else the same:\n- [Shot 4] at 00:11.000 is past the end: a 10 s clip is 10.13 s long";
        let job = WriteJob::followup_of(&b, quoting, GenParams::prompt_writer());
        assert_eq!(job.lint_ctx.duration_s, Some(10.0));
        let c = finish_with(&job, chat_result(CLEAN_R2V), &FAKE_LINTER, save);
        let c = history::load_in(&dir, c.history_id.as_deref().unwrap()).unwrap();
        assert_eq!((c.duration_s, c.lint_context.as_ref().and_then(|x| x.duration_s)), (Some(10), Some(10.0)));
        assert_eq!(c.lint_context.as_ref().and_then(|x| x.ref_count), Some(2));

        // A fix that switches the mode: the next fix uses f2v and no r2v picture facts leak.
        let job =
            WriteJob::followup_of(&c, "Rewrite it as an f2v first-frame prompt (I2VA).", GenParams::prompt_writer());
        let d = finish_with(&job, chat_result(CLEAN_F2V), &FAKE_LINTER, save);
        let d = history::load_in(&dir, d.history_id.as_deref().unwrap()).unwrap();
        let next = WriteJob::followup_of(&d, "Remove the music.", GenParams::prompt_writer());
        assert_eq!(next.mode, Mode::F2v);
        assert_eq!(
            next.lint_ctx,
            LintContext { mode: Mode::F2v, duration_s: Some(10.0), ..LintContext::default() },
            "no ref count, pin or last frame from the r2v form"
        );
        let filled =
            request::fill_missing(next.lint_ctx.clone(), &request::context_from_text(d.mode, d.original_request()));
        assert_eq!((filled.ref_count, filled.has_last_frame), (Some(1), Some(false)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn followups_send_the_pictures_that_can_still_be_read() {
        let dir = temp_dir("followup-pictures");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("ref1.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        let picture = |path: PathBuf| crate::request::RefImage {
            description: "a woman".into(),
            image_path: Some(path),
            send_image: true,
            ..Default::default()
        };
        let spec = RequestSpec {
            refs: vec![picture(png.clone()), picture(dir.join("moved-away.png"))],
            action: "she waves".into(),
            ..RequestSpec::default()
        };
        let mut entry = HistoryEntry::new(Mode::R2v, &spec.build_text(), CLEAN_R2V);
        entry.spec = Some(spec);
        let job = WriteJob::followup_of(&entry, "Remove the music.", GenParams::prompt_writer());
        assert_eq!(job.images.len(), 1);
        let previous = &job.followup.as_ref().unwrap().previous_request;
        assert!(previous.contains("\nAttached pictures, in this order: Ref 1.\n"), "{previous}");

        // Pictures of a typed request are saved with the entry and sent again.
        let mut typed = WriteJob::from_text(Mode::R2v, "Ref 1: a woman. 6 s, one take.", GenParams::prompt_writer());
        typed.images.push(ImageAttachment::from_file(&png).unwrap());
        let ctx = LintContext { mode: Mode::R2v, ..LintContext::default() };
        let saved = build_history_entry(&typed, &chat_result(CLEAN_R2V), CLEAN_R2V, &ctx, &report(Mode::R2v, 100, 0));
        assert_eq!(saved.image_paths, vec![png.clone()]);
        let again = WriteJob::followup_of(&saved, "Remove the music.", GenParams::prompt_writer());
        assert_eq!(again.images.len(), 1);
        assert!(entry.image_paths.is_empty(), "a form keeps its own pictures");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lint_context_uses_the_job_mode() {
        let mut job = WriteJob::from_text(Mode::F2v, "Picture 1: a man.", GenParams::prompt_writer());
        job.lint_ctx =
            LintContext { mode: Mode::R2v, duration_s: Some(6.0), ref_count: Some(1), ..LintContext::default() };
        let ctx = lint_context_for(&job, CLEAN_F2V);
        assert_eq!(ctx.mode, Mode::F2v);
        assert_eq!(ctx.duration_s, Some(6.0));
        assert_eq!(ctx.ref_count, Some(1));
    }

    #[test]
    fn followup_job_from_history_entry() {
        let request = format!("Picture 1: a man.{}Make it longer.", history::FIX_MARKER);
        let mut entry = HistoryEntry::new(Mode::F2v, &request, CLEAN_F2V);
        entry.duration_s = Some(6);
        let job = WriteJob::followup_of(&entry, "Remove the music.", GenParams::prompt_writer());
        assert_eq!(job.mode, Mode::F2v);
        assert_eq!(job.request, "Remove the music.");
        assert_eq!(job.lint_ctx.mode, Mode::F2v);
        assert_eq!(job.lint_ctx.duration_s, Some(6.0));
        assert!(job.images.is_empty() && job.spec.is_none() && job.save_history);
        let followup = job.followup.as_ref().unwrap();
        assert_eq!(followup.previous_request, "Picture 1: a man.");
        assert_eq!(followup.previous_output, CLEAN_F2V);
        assert_eq!(followup.fix, "Remove the music.");
        assert_eq!(followup.parent_id.as_deref(), Some(entry.id.as_str()));
        let msgs = build_messages(&Library::builtin(), &job);
        assert_eq!(msgs[1].content, "Picture 1: a man.");
        assert_eq!(msgs[2].content, CLEAN_F2V);
        assert_eq!(msgs[3].content, "Remove the music.");
    }

    fn report(mode: Mode, score: u8, errors: usize) -> LintReport {
        use crate::lint::{LintIssue, Severity};
        LintReport {
            mode,
            ok: errors == 0,
            score,
            word_count: 120,
            description_words: 90,
            shots: Vec::new(),
            issues: (0..errors)
                .map(|i| LintIssue {
                    severity: Severity::Error,
                    code: format!("error-{i}"),
                    message: "m".into(),
                    line: None,
                })
                .collect(),
        }
    }

    #[test]
    fn history_entry_from_a_job_without_the_linter() {
        let mut job = WriteJob::from_text(Mode::F2v, "Picture 1: a man.\nHe sits.", GenParams::prompt_writer());
        let result = chat_result(CLEAN_F2V);
        let ctx = LintContext { mode: Mode::F2v, duration_s: Some(5.5), ..LintContext::default() };
        let entry = build_history_entry(&job, &result, CLEAN_F2V, &ctx, &report(Mode::F2v, 73, 1));
        assert_eq!(entry.mode, Mode::F2v);
        assert_eq!(entry.request, "Picture 1: a man.\nHe sits.");
        assert_eq!(entry.title, "Picture 1: a man.");
        assert_eq!(entry.output, CLEAN_F2V);
        assert_eq!(entry.reasoning, "thought");
        assert_eq!(entry.params, job.params);
        assert_eq!(entry.timings, result.timings);
        assert_eq!((entry.lint_score, entry.lint_errors), (Some(73), 1));
        assert_eq!(entry.duration_s, Some(6));
        assert_eq!(entry.parent_id, None);
        assert!(!entry.starred);

        // Follow-up: earlier request + marker + fix, a "Fix: ..." title and the parent id.
        job.followup = Some(FollowUp {
            previous_request: "Picture 1: a man.".into(),
            previous_output: CLEAN_F2V.into(),
            fix: "Make the detailed_description longer (about 350 words): add light, fabric and hair.".into(),
            parent_id: Some("20261028-102644-123".into()),
        });
        let entry = build_history_entry(&job, &result, CLEAN_F2V, &ctx, &report(Mode::F2v, 100, 0));
        assert_eq!(entry.original_request(), "Picture 1: a man.");
        assert_eq!(
            entry.fix_text(),
            Some("Make the detailed_description longer (about 350 words): add light, fabric and hair.")
        );
        assert_eq!(entry.title, "Fix: Make the detailed_description longer (about 350 words)...");
        assert_eq!(entry.parent_id.as_deref(), Some("20261028-102644-123"));
        assert_eq!((entry.lint_score, entry.lint_errors), (Some(100), 0));

        // Unusable durations are not stored.
        for bad in [f32::NAN, -3.0, 0.0] {
            let bad_ctx = LintContext { duration_s: Some(bad), ..ctx.clone() };
            assert_eq!(
                build_history_entry(&job, &result, CLEAN_F2V, &bad_ctx, &report(Mode::F2v, 1, 0)).duration_s,
                None
            );
        }
    }

    /// Stand-in for `lint::lint`: score 88, one error when the length is unknown.
    fn fake_lint(text: &str, ctx: &LintContext) -> LintReport {
        let mut r = report(ctx.mode, 88, usize::from(ctx.duration_s.is_none()));
        r.word_count = text.split_whitespace().count();
        r
    }

    /// Stand-in for `lint::detect_mode`.
    fn fake_detect(text: &str) -> Option<Mode> {
        if text.starts_with("subject_definitions:") {
            Some(Mode::R2v)
        } else if text.starts_with(I2VA_START) || text.contains("integrated_multimodal_description:") {
            Some(Mode::F2v)
        } else {
            None
        }
    }

    const FAKE_LINTER: Linter = Linter { lint: fake_lint, detect_mode: fake_detect };

    #[test]
    fn finish_saves_the_cleaned_prompt_and_run_facts() {
        let dir = temp_dir("finish-fake");
        let mut job = WriteJob::from_text(Mode::R2v, "Ref 1: a woman.\nShe sits down.", GenParams::prompt_writer());
        job.lint_ctx.duration_s = Some(6.4);
        let save = |e: &HistoryEntry| history::save_in(&dir, e);
        let raw = format!("Here is the prompt:\n```\n{CLEAN_R2V}\n```\nLet me know!");
        let out = finish_with(&job, chat_result(&raw), &FAKE_LINTER, save);
        assert_eq!(out.prompt, CLEAN_R2V);
        assert_eq!(out.reasoning, "thought");
        assert_eq!(out.result.content, raw);
        assert_eq!((out.lint.mode, out.lint.score, out.lint.errors()), (Mode::R2v, 88, 0));
        assert_eq!(out.lint.word_count, CLEAN_R2V.split_whitespace().count(), "the cleaned text is linted");
        let entry = history::load_in(&dir, out.history_id.as_deref().expect("saved")).unwrap();
        assert_eq!(entry.mode, Mode::R2v);
        assert_eq!(entry.request, "Ref 1: a woman.\nShe sits down.");
        assert_eq!(entry.title, "Ref 1: a woman.");
        assert_eq!(entry.output, CLEAN_R2V);
        assert_eq!(entry.reasoning, "thought");
        assert_eq!(entry.params, job.params);
        assert_eq!(entry.timings, out.result.timings);
        assert_eq!((entry.lint_score, entry.lint_errors), (Some(88), 0));
        assert_eq!(entry.duration_s, Some(6));
        assert_eq!((entry.parent_id, entry.spec), (None, None));

        // Not saved: history off, a cancelled answer, an empty answer.
        job.save_history = false;
        assert_eq!(finish_with(&job, chat_result(CLEAN_R2V), &FAKE_LINTER, save).history_id, None);
        job.save_history = true;
        let mut cancelled = chat_result(CLEAN_R2V);
        cancelled.cancelled = true;
        assert_eq!(finish_with(&job, cancelled, &FAKE_LINTER, save).history_id, None);
        let empty = finish_with(&job, chat_result(" \n```\n```"), &FAKE_LINTER, save);
        assert_eq!((empty.prompt.as_str(), empty.history_id), ("", None));
        assert_eq!(history::list_in(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_keeps_going_when_saving_fails() {
        let job = WriteJob::from_text(Mode::R2v, "Ref 1: a woman.", GenParams::prompt_writer());
        let out = finish_with(&job, chat_result(CLEAN_R2V), &FAKE_LINTER, |_| Err(anyhow::anyhow!("disk full")));
        assert_eq!(out.history_id, None);
        assert_eq!(out.prompt, CLEAN_R2V);
        assert_eq!(out.lint.score, 88);
    }

    #[test]
    fn finish_followup_records_parent_fix_and_new_length() {
        let dir = temp_dir("finish-followup");
        let mut job = WriteJob::from_text(Mode::R2v, "Make it 10 s with 2 cuts.", GenParams::prompt_writer());
        job.lint_ctx =
            LintContext { mode: Mode::R2v, duration_s: Some(6.0), ref_count: Some(2), ..LintContext::default() };
        job.followup = Some(FollowUp {
            previous_request: "Ref 1: a woman.".into(),
            previous_output: CLEAN_R2V.into(),
            fix: "Make it 10 s with 2 cuts.".into(),
            parent_id: Some("20261028-102644-123".into()),
        });
        let ctx = context_with(&job, CLEAN_R2V, fake_detect);
        assert_eq!((ctx.mode, ctx.duration_s, ctx.ref_count), (Mode::R2v, Some(10.0), Some(2)));
        let out = finish_with(&job, chat_result(CLEAN_R2V), &FAKE_LINTER, |e: &HistoryEntry| history::save_in(&dir, e));
        let entry = history::load_in(&dir, out.history_id.as_deref().unwrap()).unwrap();
        assert_eq!(entry.parent_id.as_deref(), Some("20261028-102644-123"));
        assert_eq!(entry.original_request(), "Ref 1: a woman.");
        assert_eq!(entry.fix_text(), Some("Make it 10 s with 2 cuts."));
        assert_eq!(entry.title, "Fix: Make it 10 s with 2 cuts.");
        assert_eq!(entry.duration_s, Some(10));
        assert_eq!((entry.mode, out.lint.mode), (Mode::R2v, Mode::R2v));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn followup_rewritten_as_the_other_kind_is_linted_as_that_kind() {
        let dir = temp_dir("finish-rewrite");
        let fix = "Rewrite it as an f2v first-frame prompt (I2VA) instead.";
        let mut job = WriteJob::from_text(Mode::R2v, fix, GenParams::prompt_writer());
        job.lint_ctx = LintContext {
            mode: Mode::R2v,
            duration_s: Some(6.0),
            ref_count: Some(2),
            first_frame_pinned: Some(false),
            ..LintContext::default()
        };
        // An empty `fix` falls back to the job's request.
        job.followup = Some(FollowUp {
            previous_request: "Ref 1: a woman.".into(),
            previous_output: CLEAN_R2V.into(),
            fix: String::new(),
            parent_id: None,
        });
        let ctx = context_with(&job, CLEAN_F2V, fake_detect);
        assert_eq!(ctx, LintContext { mode: Mode::F2v, duration_s: Some(6.0), ..LintContext::default() });
        let out = finish_with(&job, chat_result(CLEAN_F2V), &FAKE_LINTER, |e: &HistoryEntry| history::save_in(&dir, e));
        assert_eq!(out.lint.mode, Mode::F2v);
        let entry = history::load_in(&dir, out.history_id.as_deref().unwrap()).unwrap();
        assert_eq!(entry.mode, Mode::F2v);
        assert_eq!(entry.title, format!("Fix: {fix}"));
        assert_eq!(entry.fix_text(), Some(fix));

        // A plain job is always linted as its own kind.
        job.followup = None;
        assert_eq!(context_with(&job, CLEAN_F2V, fake_detect).mode, Mode::R2v);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The tests below run the real `lint::lint`.

    #[test]
    fn finish_cleans_lints_and_skips_history_when_off() {
        let mut job = WriteJob::from_text(Mode::R2v, "Ref 1: a woman.", GenParams::prompt_writer());
        job.save_history = false;
        let raw = format!("Here is the prompt:\n```\n{CLEAN_R2V}\n```");
        let out = finish(&job, chat_result(&raw));
        assert_eq!(out.prompt, CLEAN_R2V);
        assert_eq!(out.reasoning, "thought");
        assert_eq!(out.lint.mode, Mode::R2v);
        assert_eq!(out.history_id, None);
        assert_eq!(out.result.content, raw);
    }

    #[test]
    fn finish_in_saves_the_history_entry() {
        let dir = temp_dir("finish");
        let mut job = WriteJob::from_text(Mode::R2v, "Ref 1: a woman.\nShe sits down.", GenParams::prompt_writer());
        job.lint_ctx.duration_s = Some(6.4);
        let out = finish_in(&job, chat_result(CLEAN_R2V), &dir);
        let id = out.history_id.clone().expect("saved to history");
        let entry = history::load_in(&dir, &id).unwrap();
        assert_eq!(entry.mode, Mode::R2v);
        assert_eq!(entry.request, "Ref 1: a woman.\nShe sits down.");
        assert_eq!(entry.title, "Ref 1: a woman.");
        assert_eq!(entry.output, CLEAN_R2V);
        assert_eq!(entry.reasoning, "thought");
        assert_eq!(entry.params, job.params);
        assert_eq!(entry.timings, out.result.timings);
        assert_eq!(entry.lint_score, Some(out.lint.score));
        assert_eq!(entry.lint_errors, out.lint.errors());
        assert_eq!(entry.duration_s, Some(6));
        assert_eq!(entry.parent_id, None);
        assert_eq!(entry.spec, None);

        // Cancelled and empty answers are not saved.
        let mut cancelled = chat_result(CLEAN_R2V);
        cancelled.cancelled = true;
        assert_eq!(finish_in(&job, cancelled, &dir).history_id, None);
        let empty = finish_in(&job, chat_result("  \n"), &dir);
        assert_eq!(empty.history_id, None);
        assert_eq!(empty.prompt, "");
        assert_eq!(history::list_in(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_in_keeps_going_when_history_cannot_be_saved() {
        let dir = temp_dir("finish-blocked");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-folder");
        std::fs::write(&blocker, "x").unwrap();
        let job = WriteJob::from_text(Mode::R2v, "Ref 1: a woman.", GenParams::prompt_writer());
        let out = finish_in(&job, chat_result(CLEAN_R2V), &blocker);
        assert_eq!(out.history_id, None);
        assert_eq!(out.prompt, CLEAN_R2V);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_lint_on_a_followup() {
        let mut job = WriteJob::from_text(Mode::R2v, "Make it 10 s with 2 cuts.", GenParams::prompt_writer());
        job.save_history = false;
        job.followup = Some(FollowUp {
            previous_request: "Ref 1: a woman.".into(),
            previous_output: CLEAN_R2V.into(),
            fix: "Make it 10 s with 2 cuts.".into(),
            parent_id: None,
        });
        assert_eq!(lint_context_for(&job, CLEAN_R2V).duration_s, Some(10.0));
        let out = finish(&job, chat_result(CLEAN_R2V));
        assert_eq!((out.lint.mode, out.history_id), (Mode::R2v, None));
    }
}
