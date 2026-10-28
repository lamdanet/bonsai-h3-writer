//! Checks a written H3 prompt against the official guides (and the laptop rules).
//!
//! r2v (reference mode) rules - see `assets/library/guide_r2v.md`:
//! - six sections in this order, each written as `name:` on its own line (NOT markdown like
//!   `**subject_definitions**`): subject_definitions, summary, retention_analysis,
//!   detailed_description, overall_soundscape, non_diegetic_music.
//! - summary starts with a task tag: `[reference generation]`, `[keyframe completion]` or both
//!   joined with ` + ` (no other tags for image-only use; `video editing`, `audio reuse`... are
//!   errors here because this app has no video/audio references).
//! - `[keyframe completion]`, `<Picture N> is the first frame`, `the shot begins from <Picture N>`
//!   only when a picture is a real frame (context `first_frame_pinned` / last frame).
//! - `<Picture N>` numbers must not exceed the reference count (max 9).
//! - retention lines: `<Subject N> (appears in [Shot 1], ...): marker - ...` with marker in
//!   fully_preserved | partially_preserved | attribute_transfer | weak_reference;
//!   `<Picture N> ([Shot 1] first frame): marker - ...`; no `(S1)` speaker ids in this section.
//! - detailed_description: style sentence(s) before `[Shot 1]` (should start "The target video is");
//!   `[Shot 1]` has no time; later shots open with `[Shot N] At MM:SS.mmm,`; times strictly
//!   increase and stay inside the real clip length; no `At MM:SS` / `Around 00:05` inside a shot;
//!   350-500 words is the guide's target (warning below 200, info below 350).
//! - each `<Subject N>` used later must be defined in subject_definitions.
//! - overall_soundscape 1-4 sentences, non_diegetic_music 1-3 sentences or `N/A`.
//!
//! f2v rules - see `assets/library/guide_f2v.md`:
//! - line 1 is exactly the I2VA line `For the target video, at 0.00 seconds into the target video,
//!   <Picture 1> (from [Shot 1]) is fully referenced.` or the FL2VA line `How the reference pictures
//!   align with the target video — Picture 1 (from Shot 1) aligns with the 0.00-second mark of the
//!   target video; Picture 2 (from Shot N) aligns with the S.SS-second mark of the target video.`
//!   (S.SS = [`crate::lengths::format_real`] of the duration; N = last shot), then a blank line.
//! - FL2VA iff the context has a last frame.
//! - then `integrated_multimodal_description: [Shot 1] ...`, `overall_soundscape:`, `non_diegetic_music:`.
//! - no r2v words (`<Subject`, subject_definitions, summary:, retention_analysis, detailed_description).
//! - same shot/time rules as r2v.
//!
//! Both: `No subtitles, captions, text, watermarks or logos.` recommended (info; an app rule, not in
//! the official guides); a `[Shot N]` opens a shot only at a line or sentence start (others are
//! references); quoted on-screen text may hold times; dialogue must use
//! `<d>[Language] ...</d>` with a speaker id `(S1)` before it.
//!
//! # Issue codes
//!
//! Codes are stable (scripts and the CLI's `--json` output match on them). Score = 100 - 25 per
//! error - 8 per warning - 2 per info, floored at 0. Facts missing from the [`LintContext`] skip
//! the checks that need them.
//!
//! | Code | Severity | Mode | Meaning |
//! |---|---|---|---|
//! | `empty-prompt` | error | both | nothing to check |
//! | `wrong-mode` | error | both | the text is a prompt of the other mode |
//! | `internal` | error | both | the checker itself could not run |
//! | `markdown-headings` | error | both | `**summary**` / `### summary` instead of `summary:` (sections are still parsed) |
//! | `missing-section` | error | both | a required section / field is missing |
//! | `duplicate-section` | error | both | a section appears twice |
//! | `section-order` | error | both | sections are not in the required order |
//! | `empty-section` | error | both | a section has no text |
//! | `text-before-sections` | warning | both | lead-in text before the first section (f2v: besides line 1) |
//! | `text-after-sections` | warning | both | notes after the last section, separated by a blank line |
//! | `missing-task-tag` | error | r2v | summary does not start with a `[task]` tag |
//! | `bad-task-tag` | error | r2v | unknown, repeated or video/audio-only task tag |
//! | `keyframe-without-frame` | error | r2v | keyframe tag / "is the first frame" / "begins from <Picture" with the First-frame pin off |
//! | `pinned-frame-not-used` | warning | r2v | Ref 1 is pinned but the tag / first-frame line / "begins from" is missing |
//! | `first-frame-picture` | error | r2v | the first frame is a picture other than `<Picture 1>` |
//! | `keyframe-tag-unused` | warning | r2v | `[keyframe completion]` but no picture is named as a frame |
//! | `picture-out-of-range` | error | both | `<Picture N>` above the loaded pictures (or 0, or above 9 / 2) |
//! | `unused-picture` | warning | both | a loaded picture is never cited |
//! | `no-subjects` | error | r2v | subject_definitions defines no `<Subject N>` |
//! | `undefined-subject` | error | r2v | `<Subject N>` used but not defined |
//! | `unused-subject` | info | r2v | a defined subject never appears in detailed_description |
//! | `missing-retention` | warning | r2v | a defined label has no retention_analysis line |
//! | `retention-format` | warning | r2v | not `<Subject N> (appears in [Shot 1]): marker - ...` |
//! | `bad-retention-marker` | error | r2v | marker is not fully_preserved / partially_preserved / attribute_transfer / weak_reference |
//! | `speaker-id-in-retention` | error | r2v | `(S1)` ids in retention_analysis |
//! | `unknown-shot` | warning | both | `[Shot N]` cited (r2v sections, or a reference inside the description) but there is no such shot |
//! | `shot-count-mismatch` | warning | r2v | the summary's shot count differs from the description |
//! | `missing-style-sentence` | warning | r2v | no style sentence before `[Shot 1]` |
//! | `style-sentence-start` | info | r2v | the style sentence does not start "The target video is" |
//! | `missing-shot-1` | error | both | no `[Shot 1]` in the description |
//! | `f2v-description-start` | warning | f2v | integrated_multimodal_description does not start with `[Shot 1]` |
//! | `shot-numbering` | error | both | shots are not numbered 1, 2, 3 ... |
//! | `shot1-time` | error | both | `[Shot 1]` has a time |
//! | `shot-time-missing` | error | both | a later shot has no time |
//! | `shot-time-format` | error | both | a later shot's time is not `At MM:SS.mmm,` (e.g. `Around 00:05.000`) |
//! | `shot-time-order` | error | both | shot times do not increase |
//! | `shot-time-past-end` | error | both | a shot time is at or past the real clip length |
//! | `short-shot` | info | both | a shot lasts under 1 s |
//! | `many-cuts` | info | both | more cuts than [`crate::lengths::suggested_max_cuts`] |
//! | `time-inside-shot` | warning | both | a time inside a shot ("At 00:01.200, she ...") |
//! | `cut-without-shot` | warning | both | "cuts to" inside a shot instead of a new `[Shot N]` |
//! | `single-take-with-shots` | error | both | "single continuous take" / "one take" (or "no cuts" in the summary / style text) but more than one shot |
//! | `description-short` | warning | both | description under 200 words (f2v: 100) |
//! | `description-below-target` | info | both | under 350 words (f2v: 150) |
//! | `description-long` | info | both | over 600 words (f2v: 400) |
//! | `no-subtitles-line` | info | both | "No subtitles, captions, text, watermarks or logos." (this app's rule) is missing |
//! | `soundscape-long` | info | both | overall_soundscape over 4 sentences |
//! | `music-long` | info | both | non_diegetic_music over 3 sentences |
//! | `dialogue-in-soundscape` | warning | both | speech in overall_soundscape |
//! | `dialogue-unclosed` | error | both | `<d>` without `</d>` |
//! | `dialogue-language` | warning | both | `<d>` without a `[Language]` tag |
//! | `dialogue-speaker` | warning | both | `<d>` without a `(S1)` id before it |
//! | `dialogue-not-tagged` | warning | both | quoted speech (`she says "..."`) outside `<d>` (not `a sign that says "..."`) |
//! | `f2v-words-in-r2v` | error | r2v | an f2v field or alignment line in an r2v prompt |
//! | `r2v-words-in-f2v` | error | f2v | `<Subject`, subject_definitions, summary: ... in an f2v prompt |
//! | `f2v-line1-missing` | error | f2v | line 1 is not the I2VA / FL2VA alignment line |
//! | `f2v-line1-format` | error | f2v | line 1 is not exactly the I2VA / FL2VA form |
//! | `f2v-line1-mismatch` | error | f2v | I2VA with a last frame, or FL2VA without one |
//! | `f2v-line1-blank` | warning | f2v | no blank line after line 1 |
//! | `f2v-line1-dash` | info | f2v | the FL2VA line has no em dash |
//! | `fl2va-length` | error | f2v | FL2VA seconds differ from [`crate::lengths::format_real`] of the duration |
//! | `fl2va-shot` | error | f2v | FL2VA `Picture 2 (from Shot N)` is not the last shot |
//! | `fl2va-ending` | warning | f2v | with a last frame, the description never reaches Picture 2 |

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::gen::Mode;
use crate::lengths;

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Breaks the H3 format: fix before rendering.
    Error,
    /// Likely to hurt the video.
    Warning,
    /// A hint from the guides.
    Info,
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LintIssue {
    pub severity: Severity,
    /// Stable kebab-case id, e.g. `missing-section`, `markdown-headings`, `keyframe-without-frame`.
    pub code: String,
    pub message: String,
    /// 1-based line number when it applies.
    pub line: Option<usize>,
}

/// What the prompt is for (all optional: unknown facts skip their checks).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LintContext {
    pub mode: Mode,
    /// Requested duration in seconds (real length is derived).
    pub duration_s: Option<f32>,
    /// Number of reference images loaded (r2v) or frames given (f2v).
    pub ref_count: Option<usize>,
    /// r2v: Ref 1 is pinned as the first frame (First-frame pin ON).
    pub first_frame_pinned: Option<bool>,
    /// f2v: a last frame image is used (FL2VA).
    pub has_last_frame: Option<bool>,
}

/// A shot found in the description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShotInfo {
    pub number: u32,
    /// Cut time in seconds (`None` for `[Shot 1]`).
    pub time_s: Option<f32>,
    pub words: usize,
}

/// Full result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LintReport {
    pub mode: Mode,
    /// No errors.
    pub ok: bool,
    /// 100 minus 25 per error, 8 per warning, 2 per info (floored at 0).
    pub score: u8,
    pub word_count: usize,
    /// Words in detailed_description (r2v) / integrated_multimodal_description (f2v).
    pub description_words: usize,
    pub shots: Vec<ShotInfo>,
    pub issues: Vec<LintIssue>,
}

impl LintReport {
    /// Number of error issues.
    pub fn errors(&self) -> usize {
        self.issues.iter().filter(|i| i.severity == Severity::Error).count()
    }
    /// Number of warning issues.
    pub fn warnings(&self) -> usize {
        self.issues.iter().filter(|i| i.severity == Severity::Warning).count()
    }
    /// e.g. `score 84 - 0 errors, 2 warnings`.
    pub fn summary(&self) -> String {
        format!("score {} - {} errors, {} warnings", self.score, self.errors(), self.warnings())
    }
}

/// The six r2v sections, in the required order.
pub const R2V_SECTIONS: [&str; 6] = [
    "subject_definitions",
    "summary",
    "retention_analysis",
    "detailed_description",
    "overall_soundscape",
    "non_diegetic_music",
];

/// The three f2v fields after line 1, in the required order.
pub const F2V_SECTIONS: [&str; 3] = ["integrated_multimodal_description", "overall_soundscape", "non_diegetic_music"];

/// The four retention markers of the reference guide.
pub const RETENTION_MARKERS: [&str; 4] =
    ["fully_preserved", "partially_preserved", "attribute_transfer", "weak_reference"];

/// The no-text sentence this app's system prompts and checklist ask for, so H3 adds no captions or
/// logos. It is an app rule: the official guides only say to quote text that is visible on screen.
pub const NO_TEXT_LINE: &str = "No subtitles, captions, text, watermarks or logos.";

/// The exact I2VA first line (f2v, first frame only).
pub const I2VA_LINE: &str =
    "For the target video, at 0.00 seconds into the target video, <Picture 1> (from [Shot 1]) is fully referenced.";

/// The FL2VA first line with placeholders (`Shot N` = last shot, `S.SS` = real length).
pub const FL2VA_TEMPLATE: &str = "How the reference pictures align with the target video — Picture 1 (from Shot 1) aligns \
     with the 0.00-second mark of the target video; Picture 2 (from Shot N) aligns with the S.SS-second mark of the target video.";

/// The exact FL2VA first line for a clip of `duration_s` requested seconds whose last shot is `last_shot`.
pub fn fl2va_line(duration_s: f32, last_shot: u32) -> String {
    format!(
        "How the reference pictures align with the target video — Picture 1 (from Shot 1) aligns with the 0.00-second \
         mark of the target video; Picture 2 (from Shot {last_shot}) aligns with the {}-second mark of the target video.",
        lengths::format_real(duration_s)
    )
}

/// Sections that only exist in r2v prompts.
const R2V_ONLY: [&str; 4] = ["subject_definitions", "summary", "retention_analysis", "detailed_description"];

/// All section names the parser recognises (both modes).
const SECTION_NAMES: [&str; 7] = [
    "subject_definitions",
    "summary",
    "retention_analysis",
    "detailed_description",
    "integrated_multimodal_description",
    "overall_soundscape",
    "non_diegetic_music",
];

/// Lint `text` as a prompt of `ctx.mode`.
pub fn lint(text: &str, ctx: &LintContext) -> LintReport {
    let mut report = LintReport {
        mode: ctx.mode,
        ok: true,
        score: 100,
        word_count: word_count(text),
        description_words: 0,
        shots: Vec::new(),
        issues: Vec::new(),
    };
    let re = match patterns() {
        Ok(re) => re,
        Err(err) => {
            report.issues.push(new_issue(
                Severity::Error,
                "internal",
                format!("the checker could not start: {err}"),
                None,
            ));
            return finish(report);
        }
    };
    if text.trim().is_empty() {
        report.issues.push(new_issue(Severity::Error, "empty-prompt", "the prompt is empty".to_string(), None));
        return finish(report);
    }
    let (r2v_marked, f2v_marked) = mode_markers(text);
    let (own, other, found) = match ctx.mode {
        Mode::R2v => (r2v_marked, f2v_marked, Mode::F2v),
        Mode::F2v => (f2v_marked, r2v_marked, Mode::R2v),
    };
    if !own && other {
        let message = format!(
            "this is an {found} prompt, but an {} prompt was asked for: lint it as {found} or rewrite it",
            ctx.mode
        );
        report.issues.push(new_issue(Severity::Error, "wrong-mode", message, Some(1)));
        return finish(report);
    }
    let sections = parse_sections(text, re);
    let mut linter = Linter::new(text, ctx, re);
    let (description_words, shots) = match ctx.mode {
        Mode::R2v => linter.r2v(&sections),
        Mode::F2v => linter.f2v(&sections),
    };
    report.description_words = description_words;
    report.shots = shots;
    report.issues = linter.issues;
    finish(report)
}

/// Guess the prompt kind from its text: the I2VA or FL2VA first line -> f2v (it decides, so a stray
/// r2v word such as `detailed_description` in an f2v prompt does not flip it), else an r2v section
/// name (`subject_definitions:` ...) -> r2v, else `integrated_multimodal_description` -> f2v.
pub fn detect_mode(text: &str) -> Option<Mode> {
    if alignment_first_line(text) {
        return Some(Mode::F2v);
    }
    match mode_markers(text) {
        (true, _) => Some(Mode::R2v),
        (false, true) => Some(Mode::F2v),
        (false, false) => None,
    }
}

/// (has r2v section names, has f2v field / alignment line).
fn mode_markers(text: &str) -> (bool, bool) {
    let lower = text.to_ascii_lowercase();
    let r2v = ["subject_definitions", "retention_analysis", "detailed_description"].iter().any(|w| lower.contains(w));
    let f2v = lower.contains("integrated_multimodal_description") || alignment_first_line(text);
    (r2v, f2v)
}

/// The first non-empty line is the I2VA or FL2VA alignment line.
fn alignment_first_line(text: &str) -> bool {
    text.lines().map(str::trim).find(|l| !l.is_empty()).is_some_and(is_alignment_line)
}

/// `For the target video, at 0.00 seconds ...` (I2VA) / `How the reference pictures align ...` (FL2VA).
fn is_alignment_line(line: &str) -> bool {
    let lower = line.trim_start().to_ascii_lowercase();
    lower.starts_with("for the target video, at 0.00 seconds") || lower.starts_with("how the reference pictures align")
}

/// Sort issues (errors first, then by line) and compute score / ok.
fn finish(mut report: LintReport) -> LintReport {
    report.issues.sort_by_key(|i| (i.severity, i.line.unwrap_or(usize::MAX)));
    let count = |sev: Severity| report.issues.iter().filter(|i| i.severity == sev).count();
    let (errors, warnings, infos) = (count(Severity::Error), count(Severity::Warning), count(Severity::Info));
    let penalty = errors * 25 + warnings * 8 + infos * 2;
    report.score = u8::try_from(100usize.saturating_sub(penalty)).unwrap_or(0);
    report.ok = errors == 0;
    report
}

fn new_issue(severity: Severity, code: &str, message: String, line: Option<usize>) -> LintIssue {
    LintIssue { severity, code: code.to_string(), message, line }
}

// ---------------------------------------------------------------------------------------------
// Patterns

struct Patterns {
    header: Regex,
    subject: Regex,
    subject_def: Regex,
    picture: Regex,
    picture_line: Regex,
    first_frame_line: Regex,
    begins_from: Regex,
    other_frame: Regex,
    task_tag: Regex,
    retention: Regex,
    marker: Regex,
    speaker: Regex,
    shot: Regex,
    strict_time: Regex,
    loose_time: Regex,
    inner_time: Regex,
    single_take: Regex,
    no_cuts: Regex,
    shot_count: Regex,
    lang_tag: Regex,
    untagged_speech: Regex,
    cut_words: Regex,
    r2v_words: Regex,
    f2v_words: Regex,
    fl2va: Regex,
}

impl Patterns {
    fn compile() -> Result<Self, regex::Error> {
        Ok(Self {
            header: Regex::new(
                r"(?i)^[ \t]*(?P<pre>#{1,6}[ \t]*|\*\*|__)?[ \t]*(?P<name>subject_definitions|summary|retention_analysis|detailed_description|integrated_multimodal_description|overall_soundscape|non_diegetic_music)[ \t]*(?P<post>\*\*|__)?[ \t]*(?P<colon>:)?[ \t]*(?P<post2>\*\*|__)?[ \t]*(?P<rest>.*?)[ \t]*$",
            )?,
            subject: Regex::new(r"<Subject\s*(\d+)>")?,
            subject_def: Regex::new(r"(?m)^[ \t]*(?:[-*][ \t]*)?<Subject\s*(\d+)>|<Subject\s*(\d+)>\s+(?:is|are)\b")?,
            picture: Regex::new(r"\bPicture\s*(\d+)")?,
            picture_line: Regex::new(r"(?m)^[ \t]*(?:[-*][ \t]*)?<Picture\s*(\d+)>")?,
            first_frame_line: Regex::new(r"(?i)<Picture\s*(\d+)>\s+is\s+the\s+first\s+frame")?,
            begins_from: Regex::new(
                r"(?i)\b(?:begins?|starts?|opens?)\s+(?:exactly\s+)?(?:from|on|with)\s+<Picture\s*(\d+)>",
            )?,
            other_frame: Regex::new(
                r"(?i)<Picture\s*\d+>\s+is\s+(?:the|a)\s+(?:keyframe|key\s+frame|last\s+frame|final\s+frame|edited\s+keyframe)|keyframe\s+corresponds\s+to\s+<Picture|\bends\s+on\s+<Picture",
            )?,
            task_tag: Regex::new(r"^\[([^\]\n]*)\]")?,
            retention: Regex::new(
                r"^[ \t]*(?:[-*][ \t]*)?<(Subject|Picture)\s*(\d+)>[ \t]*(\([^)\n]*\))?[ \t]*:?[ \t]*(.*)$",
            )?,
            marker: Regex::new(r"^([A-Za-z][A-Za-z_ ]*?)[ \t]*(?:[-–—:;,.(]|$)")?,
            speaker: Regex::new(r"\(S\d+\)")?,
            shot: Regex::new(r"(?i)\[Shot\s*(\d+)\]")?,
            strict_time: Regex::new(r"^[ \t]*At[ \t]+(\d{2}:\d{2}\.\d{3}),")?,
            loose_time: Regex::new(
                r"(?i)^[ \t]*[(\[]?[ \t]*(?:(?:at|around|from|about|approximately|approx\.?|by|after|~)[ \t]*)?(\d{1,2}:\d{2}(?:[.,]\d{1,3})?|\d+(?:\.\d+)?[ \t]*(?:s|sec|secs|seconds)\b)[)\]]?,?",
            )?,
            inner_time: Regex::new(
                r"(?i)\b\d{1,2}:\d{2}(?:[.,]\d{1,3})?\b|\b(?:at|around|by|until|after)\s+(?:the\s+)?\d+(?:\.\d+)?\s*(?:s|sec|secs|seconds)\b",
            )?,
            single_take: Regex::new(
                r"(?i)\b(?:single[\s-]+(?:continuous[\s-]+|unbroken[\s-]+)?(?:take|shot)|one[\s-]+(?:single[\s-]+)?(?:continuous[\s-]+|unbroken[\s-]+)?take|continuous[\s-]+single[\s-]+take|unbroken[\s-]+(?:take|shot))\b",
            )?,
            no_cuts: Regex::new(r"(?i)\b(?:no[\s-]+cuts?|without[\s-]+(?:any[\s-]+)?(?:cuts?|cutting))\b")?,
            shot_count: Regex::new(
                r"(?i)\b(?:in|across|over|with)\s+(one|two|three|four|five|six|seven|eight|nine|ten|\d+)\s+(?:separate\s+|distinct\s+)?shots?\b|\b(one|two|three|four|five|six|seven|eight|nine|ten|\d+)-shot\s+(?:[a-z]+\s+){0,2}?(?:scene|sequence|video|clip|edit|montage|story)\b",
            )?,
            lang_tag: Regex::new(r"^\s*\[[^\]\n]{1,40}\]")?,
            untagged_speech: Regex::new(
                r#"(?i)\b(?:says|said|asks|asked|answers|answered|replies|replied|whispers|whispered|shouts|shouted)\b[^"“<\n.]{0,30}["“][^"”\n]{1,300}["”]"#,
            )?,
            cut_words: Regex::new(r"(?i)\bcuts?\s+(?:back\s+)?to\b|\b(?:hard|jump)\s+cut\b")?,
            r2v_words: Regex::new(
                r"(?i)<Subject\s*\d+>|\bsubject_definitions\b|\bsummary\s*:|\bretention_analysis\b|\bdetailed_description\b|\[(?:reference generation|keyframe completion)",
            )?,
            f2v_words: Regex::new(
                r"(?im)\bintegrated_multimodal_description\b|^[ \t]*For the target video, at 0\.00 seconds|^[ \t]*How the reference pictures align",
            )?,
            fl2va: Regex::new(
                r"^How the reference pictures align with the target video ?(—|–|-{1,2}) ?Picture 1 \(from Shot 1\) aligns with the 0\.00-second mark of the target video; ?Picture 2 \(from Shot (\d+)\) aligns with the (\d+(?:\.\d+)?)-second mark of the target video\.?$",
            )?,
        })
    }
}

/// Compiled once; an error here is a bug in a pattern above (covered by a unit test).
fn patterns() -> Result<&'static Patterns, String> {
    static PATTERNS: OnceLock<Result<Patterns, String>> = OnceLock::new();
    match PATTERNS.get_or_init(|| Patterns::compile().map_err(|e| e.to_string())) {
        Ok(p) => Ok(p),
        Err(e) => Err(e.clone()),
    }
}

// ---------------------------------------------------------------------------------------------
// Sections

/// A section found in the text. Offsets are byte offsets into the whole prompt.
#[derive(Debug, Clone)]
struct Section {
    name: &'static str,
    /// 1-based line of the heading.
    line: usize,
    /// Offset of the heading line.
    header: usize,
    /// Content range (inline text after `name:` included).
    start: usize,
    end: usize,
    markdown: bool,
}

/// Find headings (`name:`, `name: text`, `**name**`, `### name` ...) of all known sections.
fn parse_sections(text: &str, re: &Patterns) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut offset = 0;
    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches(['\n', '\r']);
        if let Some(c) = re.header.captures(line) {
            let pre = c.name("pre").is_some();
            let post = c.name("post").is_some() || c.name("post2").is_some();
            let colon = c.name("colon").is_some();
            let rest = c.name("rest").filter(|m| !m.as_str().is_empty());
            let name =
                c.name("name").and_then(|m| SECTION_NAMES.iter().copied().find(|n| n.eq_ignore_ascii_case(m.as_str())));
            if let (Some(name), true) = (name, colon || post || (pre && rest.is_none())) {
                if let Some(prev) = out.last_mut() {
                    prev.end = offset;
                }
                let start = rest.map_or(offset + raw.len(), |m| offset + m.start());
                out.push(Section {
                    name,
                    line: idx + 1,
                    header: offset,
                    start,
                    end: text.len(),
                    markdown: pre || post,
                });
            }
        }
        offset += raw.len();
    }
    out
}

fn find_section<'s>(sections: &'s [Section], name: &str) -> Option<&'s Section> {
    sections.iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------------------------
// The checker

struct Linter<'a> {
    text: &'a str,
    ctx: &'a LintContext,
    re: &'static Patterns,
    line_starts: Vec<usize>,
    issues: Vec<LintIssue>,
}

impl<'a> Linter<'a> {
    fn new(text: &'a str, ctx: &'a LintContext, re: &'static Patterns) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { text, ctx, re, line_starts, issues: Vec::new() }
    }

    fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= offset).max(1)
    }

    fn push(&mut self, severity: Severity, code: &str, message: impl Into<String>, offset: Option<usize>) {
        let line = offset.map(|o| self.line_of(o));
        self.issues.push(new_issue(severity, code, message.into(), line));
    }
    fn error(&mut self, code: &str, message: impl Into<String>, offset: Option<usize>) {
        self.push(Severity::Error, code, message, offset);
    }
    fn warn(&mut self, code: &str, message: impl Into<String>, offset: Option<usize>) {
        self.push(Severity::Warning, code, message, offset);
    }
    fn info(&mut self, code: &str, message: impl Into<String>, offset: Option<usize>) {
        self.push(Severity::Info, code, message, offset);
    }

    /// Content of a section.
    fn body(&self, s: &Section) -> &'a str {
        let text: &'a str = self.text;
        &text[s.start..s.end]
    }

    // ----- r2v ----------------------------------------------------------------------------

    fn r2v(&mut self, sections: &[Section]) -> (usize, Vec<ShotInfo>) {
        let (text, re) = (self.text, self.re);
        self.structure(sections, &R2V_SECTIONS);
        if let Some(m) = re.f2v_words.find(text) {
            let msg = format!("\"{}\" belongs to f2v prompts; r2v uses the six sections only", snippet(m.as_str()));
            self.error("f2v-words-in-r2v", msg, Some(m.start()));
        }
        self.text_before(sections, false);
        let defs = find_section(sections, "subject_definitions");
        let summary = find_section(sections, "summary");
        let retention = find_section(sections, "retention_analysis");
        let desc = find_section(sections, "detailed_description");

        // Subjects and picture lines.
        let mut defined = BTreeSet::new();
        let mut picture_lines = BTreeMap::new();
        if let Some(d) = defs {
            let body = self.body(d);
            for c in re.subject_def.captures_iter(body) {
                if let Some(n) = c.get(1).or_else(|| c.get(2)).and_then(|m| m.as_str().parse::<u32>().ok()) {
                    defined.insert(n);
                }
            }
            for (offset, n) in numbered_matches(&re.picture_line, body) {
                picture_lines.entry(n).or_insert(d.start + offset);
            }
            if !body.trim().is_empty() && defined.is_empty() && picture_lines.is_empty() {
                let msg = "subject_definitions defines no <Subject N>: write \"<Subject 1> is the woman in <Picture 1>, ...\"";
                self.error("no-subjects", msg, Some(d.header));
            }
            let mut reported = BTreeSet::new();
            for (offset, n) in numbered_matches(&re.subject, text) {
                let inside_defs = offset >= d.start && offset < d.end;
                if !inside_defs && !defined.contains(&n) && reported.insert(n) {
                    let msg = format!("<Subject {n}> is used but not defined in subject_definitions");
                    self.error("undefined-subject", msg, Some(offset));
                }
            }
        }

        // Summary task tag.
        let mut keyframe_tag = false;
        let mut tag_offset = None;
        if let Some(s) = summary {
            let body = self.body(s);
            let lead = body.trim_start();
            if !lead.is_empty() {
                let lead_offset = s.start + (body.len() - lead.len());
                tag_offset = Some(lead_offset);
                keyframe_tag = self.task_tags(lead, lead_offset);
            }
        }
        self.frame_rules(keyframe_tag, tag_offset);
        self.pictures();
        if let Some(r) = retention {
            self.retention(r, &defined, &picture_lines);
        }

        let (words, shots) = match desc {
            Some(d) => self.r2v_description(d),
            None => (0, Vec::new()),
        };

        if let Some(d) = desc {
            let used: BTreeSet<u32> = numbered_matches(&re.subject, self.body(d)).into_iter().map(|(_, n)| n).collect();
            for n in defined.iter().filter(|n| !used.contains(n)) {
                let msg = format!("<Subject {n}> is defined but never appears in detailed_description");
                self.info("unused-subject", msg, defs.map(|s| s.header));
            }
        }
        let max_shot = shots.iter().map(|s| s.number).max().unwrap_or(0);
        if max_shot > 0 {
            let mut reported = BTreeSet::new();
            for s in [defs, summary, retention].into_iter().flatten() {
                for (offset, n) in numbered_matches(&re.shot, self.body(s)) {
                    if n > max_shot && reported.insert(n) {
                        let msg = format!("[Shot {n}] is cited but detailed_description has only {max_shot} shot(s)");
                        self.warn("unknown-shot", msg, Some(s.start + offset));
                    }
                }
            }
        }
        if let (Some(s), false) = (summary, shots.is_empty()) {
            let body = self.body(s);
            if let Some(c) = re.shot_count.captures(body) {
                let said = c.get(1).or_else(|| c.get(2)).and_then(|m| number_word(m.as_str()));
                let offset = c.get(0).map(|m| s.start + m.start());
                if let Some(n) = said.filter(|&n| n as usize != shots.len()) {
                    let msg = format!("the summary says {n} shot(s) but detailed_description has {}", shots.len());
                    self.warn("shot-count-mismatch", msg, offset);
                }
            }
        }
        self.single_take(shots.len(), &[summary, desc]);
        self.sound_and_music(sections);
        self.trailing_text(sections);
        self.dialogue();
        self.no_text_line();
        (words, shots)
    }

    /// Checks the leading `[tag]` of the summary; returns whether `[keyframe completion]` is used.
    fn task_tags(&mut self, lead: &str, offset: usize) -> bool {
        let Some(c) = self.re.task_tag.captures(lead) else {
            let msg = "summary must start with [reference generation] (or [keyframe completion] when a picture is a real frame)";
            self.error("missing-task-tag", msg, Some(offset));
            return false;
        };
        let inner = c.get(1).map_or("", |m| m.as_str());
        let mut seen = BTreeSet::new();
        let mut keyframe = false;
        for tag in inner.split('+').map(|t| t.trim().to_ascii_lowercase()) {
            match tag.as_str() {
                "reference generation" | "keyframe completion" => {
                    keyframe |= tag == "keyframe completion";
                    if !seen.insert(tag.clone()) {
                        let msg = format!("[{tag}] is repeated: join different tags with \" + \" and never repeat one");
                        self.error("bad-task-tag", msg, Some(offset));
                    }
                }
                _ if tag.contains("video") || tag.contains("audio") => {
                    let msg = format!("[{tag}] needs a reference video or audio; this app only uses pictures: use [reference generation]");
                    self.error("bad-task-tag", msg, Some(offset));
                }
                _ => {
                    let msg =
                        format!("unknown task tag [{tag}]: use [reference generation] and/or [keyframe completion]");
                    self.error("bad-task-tag", msg, Some(offset));
                }
            }
        }
        keyframe
    }

    /// Keyframe tag / first-frame wording against the First-frame pin.
    fn frame_rules(&mut self, keyframe_tag: bool, tag_offset: Option<usize>) {
        let (text, re) = (self.text, self.re);
        let first_lines = numbered_matches(&re.first_frame_line, text);
        let begins = numbered_matches(&re.begins_from, text);
        let other_frames = re.other_frame.is_match(text);
        let mut claims: Vec<(usize, u32)> = first_lines.iter().chain(begins.iter()).copied().collect();
        claims.sort_unstable();
        match self.ctx.first_frame_pinned {
            Some(false) => {
                if keyframe_tag && !other_frames {
                    let msg = "[keyframe completion] is only for a picture that is a real frame, and the First-frame pin is off: use [reference generation]";
                    self.error("keyframe-without-frame", msg, tag_offset);
                }
                if let Some(&(offset, n)) = claims.first() {
                    let msg = format!(
                        "<Picture {n}> is written as the first frame (\"is the first frame\" / \"begins from <Picture {n}>\"), \
                         but the First-frame pin is off: the pictures only give the look"
                    );
                    self.error("keyframe-without-frame", msg, Some(offset));
                }
            }
            Some(true) => {
                let mut missing = Vec::new();
                if !keyframe_tag {
                    missing.push("[keyframe completion] in the summary");
                }
                if !first_lines.iter().any(|&(_, n)| n == 1) {
                    missing.push("\"<Picture 1> is the first frame of [Shot 1], showing ...\" in subject_definitions");
                }
                if !begins.iter().any(|&(_, n)| n == 1) {
                    missing.push("\"the shot begins from <Picture 1>\" in [Shot 1]");
                }
                if !missing.is_empty() {
                    let msg =
                        format!("Ref 1 is pinned as the first frame, but the prompt lacks {}", missing.join("; "));
                    self.warn("pinned-frame-not-used", msg, tag_offset);
                }
                if let Some(&(offset, n)) = claims.iter().find(|&&(_, n)| n != 1) {
                    let msg = format!(
                        "<Picture {n}> is written as the first frame, but only Ref 1 (<Picture 1>) can be pinned"
                    );
                    self.error("first-frame-picture", msg, Some(offset));
                }
            }
            None => {
                if keyframe_tag && claims.is_empty() && !other_frames {
                    let msg = "[keyframe completion] is used, but no picture is named as a first, key or last frame";
                    self.warn("keyframe-tag-unused", msg, tag_offset);
                }
            }
        }
    }

    /// `<Picture N>` against the loaded pictures.
    fn pictures(&mut self) {
        let (text, re) = (self.text, self.re);
        let mode = self.ctx.mode;
        let max = match mode {
            Mode::R2v => 9,
            Mode::F2v => 2,
        };
        let loaded = self.ctx.ref_count;
        let limit = loaded.map_or(max, |c| c.min(max));
        let mut seen: BTreeMap<u32, usize> = BTreeMap::new();
        for (offset, n) in numbered_matches(&re.picture, text) {
            seen.entry(n).or_insert(offset);
        }
        for (&n, &offset) in &seen {
            if n == 0 || n as usize > limit {
                let why = match loaded {
                    _ if n == 0 => "pictures are numbered from 1".to_string(),
                    Some(c) if (n as usize) <= max => format!("only {c} picture(s) are loaded"),
                    _ => format!("{mode} uses Picture 1 to Picture {max}"),
                };
                self.error("picture-out-of-range", format!("Picture {n} is cited, but {why}"), Some(offset));
            }
        }
        if let Some(c) = loaded {
            for n in 1..=c.min(max) {
                if !seen.contains_key(&(n as u32)) {
                    self.warn(
                        "unused-picture",
                        format!("Ref {n} is loaded but the prompt never cites Picture {n}"),
                        None,
                    );
                }
            }
        }
    }

    fn retention(&mut self, s: &Section, defined: &BTreeSet<u32>, picture_lines: &BTreeMap<u32, usize>) {
        let re = self.re;
        let body = self.body(s);
        let mut subjects = BTreeSet::new();
        let mut pictures = BTreeSet::new();
        let mut speaker_reported = false;
        let mut offset = s.start;
        for raw in body.split_inclusive('\n') {
            let here = offset;
            offset += raw.len();
            let line = raw.trim_end_matches(['\n', '\r']);
            if line.trim().is_empty() {
                continue;
            }
            if let (false, Some(m)) = (speaker_reported, re.speaker.find(line)) {
                let msg = format!("speaker id {} in retention_analysis: (S1) ids belong in the shots only", m.as_str());
                self.error("speaker-id-in-retention", msg, Some(here + m.start()));
                speaker_reported = true;
            }
            let Some(c) = re.retention.captures(line) else {
                let msg =
                    "retention lines start with a label: \"<Subject 1> (appears in [Shot 1]): fully_preserved - ...\"";
                self.warn("retention-format", msg, Some(here));
                continue;
            };
            let n = c.get(2).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0);
            let label = if c.get(1).map_or("", |m| m.as_str()) == "Picture" {
                pictures.insert(n);
                format!("<Picture {n}>")
            } else {
                subjects.insert(n);
                format!("<Subject {n}>")
            };
            if c.get(3).is_none() {
                let msg = format!("{label} needs its shots in brackets: \"{label} (appears in [Shot 1]): ...\"");
                self.warn("retention-format", msg, Some(here));
            }
            let rest = c.get(4).map_or("", |m| m.as_str());
            let marker =
                re.marker.captures(rest).and_then(|m| m.get(1)).map(|m| m.as_str().trim().to_ascii_lowercase());
            let markers = RETENTION_MARKERS.join(" | ");
            match marker {
                Some(mk) if RETENTION_MARKERS.contains(&mk.as_str()) => {}
                Some(mk) => self.error(
                    "bad-retention-marker",
                    format!("{label}: \"{mk}\" is not a marker; use {markers}"),
                    Some(here),
                ),
                None => self.error("bad-retention-marker", format!("{label} has no marker; use {markers}"), Some(here)),
            }
        }
        for n in defined.difference(&subjects) {
            self.warn("missing-retention", format!("<Subject {n}> has no retention_analysis line"), Some(s.header));
        }
        for n in picture_lines.keys().filter(|n| !pictures.contains(n)) {
            let msg = format!("<Picture {n}> has its own line in subject_definitions but no retention_analysis line");
            self.warn("missing-retention", msg, Some(s.header));
        }
    }

    fn r2v_description(&mut self, d: &Section) -> (usize, Vec<ShotInfo>) {
        let re = self.re;
        let body = self.body(d);
        match re.shot.find(body) {
            None => {
                let msg = "detailed_description has no [Shot 1]: write the style sentence, then \"[Shot 1] ...\"";
                self.error("missing-shot-1", msg, Some(d.header));
            }
            Some(m) => {
                let style = body[..m.start()].trim();
                if style.is_empty() {
                    let msg = "start detailed_description with 1-2 style sentences (\"The target video is ...\") before [Shot 1]";
                    self.warn("missing-style-sentence", msg, Some(d.start));
                } else if !style.to_ascii_lowercase().starts_with("the target video is") {
                    self.info(
                        "style-sentence-start",
                        "the style sentence should start \"The target video is ...\"",
                        Some(d.start),
                    );
                }
            }
        }
        let shots = self.shots(d.start, d.end);
        let words = word_count(body);
        self.word_limits(words, d.header, Mode::R2v);
        (words, shots)
    }

    // ----- f2v ----------------------------------------------------------------------------

    fn f2v(&mut self, sections: &[Section]) -> (usize, Vec<ShotInfo>) {
        let (text, re) = (self.text, self.re);
        self.structure(sections, &F2V_SECTIONS);
        let regex_hit = re.r2v_words.find(text).map(|m| (m.start(), m.as_str().to_string()));
        let section_hit =
            sections.iter().find(|s| R2V_ONLY.contains(&s.name)).map(|s| (s.header, format!("{}:", s.name)));
        if let Some((offset, word)) = [regex_hit, section_hit].into_iter().flatten().min_by_key(|(o, _)| *o) {
            let msg = format!(
                "\"{word}\" is r2v wording: f2v prompts use line 1 + integrated_multimodal_description / overall_soundscape / \
                 non_diegetic_music, with no <Subject N>"
            );
            self.error("r2v-words-in-f2v", msg, Some(offset));
        }
        self.text_before(sections, true);
        let description = find_section(sections, "integrated_multimodal_description");
        // The suggested FL2VA line names the last shot of the description.
        let last_shot = description
            .and_then(|d| self.shot_markers(d.start, d.end).into_iter().rfind(|m| m.opens))
            .map_or(1, |m| m.number.max(1));
        let fl2va_shot = self.f2v_line1(last_shot);
        self.pictures();
        let mut words = 0;
        let mut shots = Vec::new();
        if let Some(d) = description {
            let body = self.body(d);
            if !re.shot.is_match(body) {
                self.error(
                    "missing-shot-1",
                    "integrated_multimodal_description must start with [Shot 1]",
                    Some(d.header),
                );
            } else if !body.trim_start().to_ascii_lowercase().starts_with("[shot 1]") {
                let msg =
                    "integrated_multimodal_description should start with \"[Shot 1] Live-action, cinematic, ...\"";
                self.warn("f2v-description-start", msg, Some(d.header));
            }
            shots = self.shots(d.start, d.end);
            words = word_count(body);
            self.word_limits(words, d.header, Mode::F2v);
            self.single_take(shots.len(), &[Some(d)]);
            if self.ctx.has_last_frame == Some(true) && !body.contains("Picture 2") {
                let msg = "with a last frame, end on \"settling into the pose, spacing, light and composition established by \
                           Picture 2 at the end of the shot\"";
                self.warn("fl2va-ending", msg, Some(d.header));
            }
        }
        if let (Some((n, offset)), Some(last)) = (fl2va_shot, shots.last()) {
            if n != last.number {
                let msg =
                    format!("line 1 says Picture 2 is from Shot {n}, but the last shot is [Shot {}]", last.number);
                self.error("fl2va-shot", msg, Some(offset));
            }
        }
        self.sound_and_music(sections);
        self.trailing_text(sections);
        self.dialogue();
        self.no_text_line();
        (words, shots)
    }

    /// Checks the I2VA / FL2VA line (a suggested FL2VA line names `last_shot`); returns the FL2VA
    /// `Shot N` and the line offset.
    fn f2v_line1(&mut self, last_shot: u32) -> Option<(u32, usize)> {
        let (text, re) = (self.text, self.re);
        let mut lines = Vec::new();
        let mut offset = 0;
        for raw in text.split_inclusive('\n') {
            lines.push((offset, raw.trim()));
            offset += raw.len();
        }
        let idx = lines.iter().position(|(_, l)| !l.is_empty())?;
        let (off, line) = lines[idx];
        let next_blank = lines.get(idx + 1).is_none_or(|(_, l)| l.is_empty());
        let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let lower = normalized.to_ascii_lowercase();
        let has_last = self.ctx.has_last_frame;
        let duration = self.ctx.duration_s;
        let fl2va_example = duration.map_or_else(|| FL2VA_TEMPLATE.to_string(), |d| fl2va_line(d, last_shot));
        let mut shot = None;
        if lower.starts_with("for the target video") {
            if normalized != I2VA_LINE {
                self.error("f2v-line1-format", format!("line 1 must be exactly: {I2VA_LINE}"), Some(off));
            }
            if has_last == Some(true) {
                let msg = format!("a last frame is used (FL2VA), so line 1 must be: {fl2va_example}");
                self.error("f2v-line1-mismatch", msg, Some(off));
            }
        } else if lower.starts_with("how the reference pictures align") {
            match re.fl2va.captures(&normalized) {
                Some(c) => {
                    if c.get(1).map_or("—", |m| m.as_str()) != "—" {
                        let msg =
                            "the FL2VA line uses an em dash: \"... the target video — Picture 1 (from Shot 1) ...\"";
                        self.info("f2v-line1-dash", msg, Some(off));
                    }
                    let secs = c.get(3).map_or("", |m| m.as_str());
                    if let Some(d) = duration {
                        let want = lengths::format_real(d);
                        if secs != want {
                            let msg = format!(
                                "line 1 says Picture 2 aligns with the {secs}-second mark, but a {d} s clip is really {want} s long: \
                                 write \"the {want}-second mark\""
                            );
                            self.error("fl2va-length", msg, Some(off));
                        }
                    }
                    shot = c.get(2).and_then(|m| m.as_str().parse::<u32>().ok()).map(|n| (n, off));
                }
                None => {
                    let msg = format!("line 1 must be exactly: {fl2va_example} (Shot N = the last shot)");
                    self.error("f2v-line1-format", msg, Some(off));
                }
            }
            if has_last == Some(false) {
                let msg = format!("no last frame is used (I2VA), so line 1 must be: {I2VA_LINE}");
                self.error("f2v-line1-mismatch", msg, Some(off));
            }
        } else {
            let want = if has_last == Some(true) { fl2va_example } else { I2VA_LINE.to_string() };
            self.error(
                "f2v-line1-missing",
                format!("line 1 must be the alignment line, then a blank line: {want}"),
                Some(off),
            );
            return None;
        }
        if !next_blank {
            self.warn("f2v-line1-blank", "leave one blank line after line 1", Some(off));
        }
        shot
    }

    // ----- shared -------------------------------------------------------------------------

    /// Headings: markdown, duplicates, missing, order, empty.
    fn structure(&mut self, sections: &[Section], expected: &[&'static str]) {
        if let Some(s) = sections.iter().find(|s| s.markdown) {
            let msg = format!(
                "section headings are written in markdown (line {}): write each one as `{}:` on its own line, \
                 not **{}** or ### {}",
                s.line, s.name, s.name, s.name
            );
            self.error("markdown-headings", msg, Some(s.header));
        }
        let mut seen = BTreeSet::new();
        let mut order: Vec<&Section> = Vec::new();
        for s in sections.iter().filter(|s| expected.contains(&s.name)) {
            if seen.insert(s.name) {
                order.push(s);
            } else {
                self.error("duplicate-section", format!("`{}:` appears twice", s.name), Some(s.header));
            }
        }
        let wanted = expected.join(" / ");
        for name in expected.iter().filter(|n| !seen.contains(*n)) {
            self.error("missing-section", format!("missing section `{name}:` (the order is {wanted})"), None);
        }
        let rank = |s: &Section| expected.iter().position(|n| *n == s.name).unwrap_or(0);
        if let Some(pair) = order.windows(2).find(|w| rank(w[1]) < rank(w[0])) {
            let msg = format!("`{}:` comes after `{}:`; the order is {wanted}", pair[1].name, pair[0].name);
            self.error("section-order", msg, Some(pair[1].header));
        }
        for s in &order {
            if self.body(s).trim().is_empty() {
                self.error("empty-section", format!("`{}:` is empty", s.name), Some(s.header));
            }
        }
    }

    /// Lead-in text before the first heading (f2v: the first line may be the alignment line; any
    /// other first line, such as `Here is the prompt:`, is lead-in).
    fn text_before(&mut self, sections: &[Section], f2v: bool) {
        let text = self.text;
        let end = sections.first().map_or(text.len(), |s| s.header);
        let mut offset = 0;
        let mut first = f2v;
        for raw in text[..end].split_inclusive('\n') {
            let here = offset;
            offset += raw.len();
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if std::mem::take(&mut first)
                && (lower.starts_with("for the target video") || lower.starts_with("how the reference pictures align"))
            {
                continue;
            }
            let msg = format!("text before the first section: \"{}\" - remove it", snippet(line));
            self.warn("text-before-sections", msg, Some(here));
            return;
        }
    }

    /// Notes after the last section (after a blank line).
    fn trailing_text(&mut self, sections: &[Section]) {
        let Some(last) = sections.last() else { return };
        if last.name != "non_diegetic_music" && last.name != "overall_soundscape" {
            return;
        }
        let body = self.body(last);
        let mut offset = last.start;
        let (mut seen_text, mut gap) = (false, false);
        for raw in body.split_inclusive('\n') {
            let here = offset;
            offset += raw.len();
            if raw.trim().is_empty() {
                gap |= seen_text;
                continue;
            }
            if gap {
                let msg = format!("text after `{}:`: \"{}\" - remove it", last.name, snippet(raw));
                self.warn("text-after-sections", msg, Some(here));
                return;
            }
            seen_text = true;
        }
    }

    /// `[Shot N]` markers of a description range. A marker opens a shot at the start of the range
    /// or of a line, or after a sentence end (`... she sits. [Shot 2] At ...`); the first one always
    /// does. Others refer back to a shot (`framed like the opening of [Shot 1]`).
    fn shot_markers(&self, start: usize, end: usize) -> Vec<ShotMarker> {
        let text = self.text;
        let mut out: Vec<ShotMarker> = Vec::new();
        for c in self.re.shot.captures_iter(&text[start..end]) {
            let (Some(all), Some(n)) = (c.get(0), c.get(1)) else { continue };
            let at = start + all.start();
            let opens = out.is_empty() || opens_shot(&text[start..at]);
            out.push(ShotMarker { start: at, end: start + all.end(), number: n.as_str().parse().unwrap_or(0), opens });
        }
        out
    }

    /// Shots of a description range: numbering, times, cuts; references to missing shots.
    fn shots(&mut self, start: usize, end: usize) -> Vec<ShotInfo> {
        let (text, re) = (self.text, self.re);
        let all_markers = self.shot_markers(start, end);
        let markers: Vec<(usize, usize, u32)> =
            all_markers.iter().filter(|m| m.opens).map(|m| (m.start, m.end, m.number)).collect();
        let last_number = markers.iter().map(|m| m.2).max().unwrap_or(0);
        let mut reported = BTreeSet::new();
        for m in all_markers.iter().filter(|m| !m.opens && m.number > last_number) {
            if reported.insert(m.number) {
                let msg = format!("[Shot {}] is cited but the description has only {last_number} shot(s)", m.number);
                self.warn("unknown-shot", msg, Some(m.start));
            }
        }
        let duration = self.ctx.duration_s;
        let real = duration.map(lengths::real_seconds);
        let mut shots = Vec::with_capacity(markers.len());
        let mut numbering_reported = false;
        let mut prev = 0.0_f32;
        let mut times_ok = true;
        let mut cuts_at = Vec::new();
        for (i, &(m_start, m_end, number)) in markers.iter().enumerate() {
            let seg_end = markers.get(i + 1).map_or(end, |next| next.0);
            let body = &text[m_end..seg_end];
            if number as usize != i + 1 && !numbering_reported {
                let msg = format!("[Shot {number}] should be [Shot {}]: number the shots 1, 2, 3 ...", i + 1);
                self.error("shot-numbering", msg, Some(m_start));
                numbering_reported = true;
            }
            let mut opener = 0;
            let mut time_s = None;
            if i == 0 {
                if let Some(m) = re.loose_time.find(body) {
                    let msg = format!(
                        "[Shot {number}] must not have a time (\"{}\"): times only open [Shot 2] and later",
                        m.as_str().trim()
                    );
                    self.error("shot1-time", msg, Some(m_start));
                    opener = m.end();
                }
            } else if let Some(c) = re.strict_time.captures(body) {
                time_s = c.get(1).and_then(|m| lengths::parse_timestamp(m.as_str()));
                opener = c.get(0).map_or(0, |m| m.end());
            } else if let Some(c) = re.loose_time.captures(body) {
                let found = c.get(0).map_or("", |m| m.as_str().trim());
                time_s = c.get(1).and_then(|m| parse_loose_time(m.as_str()));
                opener = c.get(0).map_or(0, |m| m.end());
                let want = lengths::shot_timestamp(time_s.unwrap_or(3.0));
                let msg = format!("[Shot {number}] opens with \"{found}\"; write \"[Shot {number}] At {want}, ...\"");
                self.error("shot-time-format", msg, Some(m_start));
            } else {
                let msg = format!(
                    "[Shot {number}] needs its cut time: \"[Shot {number}] At 00:03.000, the camera cuts to ...\""
                );
                self.error("shot-time-missing", msg, Some(m_start));
            }
            if i > 0 {
                match time_s {
                    Some(t) if t <= prev + 1e-4 => {
                        let msg = format!(
                            "[Shot {number}] at {} is not after the previous shot's start ({}): times must increase",
                            lengths::shot_timestamp(t),
                            lengths::shot_timestamp(prev)
                        );
                        self.error("shot-time-order", msg, Some(m_start));
                        times_ok = false;
                    }
                    Some(t) => {
                        if let (Some(real), Some(d)) = (real, duration) {
                            if t >= real - 1e-4 {
                                let msg = format!(
                                    "[Shot {number}] at {} is past the end: a {d} s clip is {} s long",
                                    lengths::shot_timestamp(t),
                                    lengths::format_real(d)
                                );
                                self.error("shot-time-past-end", msg, Some(m_start));
                                times_ok = false;
                            }
                        }
                        prev = t;
                        cuts_at.push(t);
                    }
                    None => times_ok = false,
                }
            }
            let rest = &body[opener..];
            let rest_offset = m_end + opener;
            // On-screen text ("4:15" on a clock) and spoken words may hold times.
            let quoted = quoted_spans(rest);
            let unquoted = |m: &regex::Match| !quoted.iter().any(|&(a, b)| m.start() >= a && m.start() < b);
            if let Some(m) = re.inner_time.find_iter(rest).find(unquoted) {
                let msg = format!(
                    "time \"{}\" inside [Shot {number}]: order actions with \"then\", \"as\", \"while\" (times only open later shots)",
                    m.as_str()
                );
                self.warn("time-inside-shot", msg, Some(rest_offset + m.start()));
            }
            let skip = if i == 0 { 0 } else { first_sentence_end(rest) };
            if let Some(m) = re.cut_words.find(&rest[skip..]) {
                let msg = format!(
                    "\"{}\" inside [Shot {number}]: start a new [Shot N] with its time for a cut, or move the camera instead",
                    m.as_str()
                );
                self.warn("cut-without-shot", msg, Some(rest_offset + skip + m.start()));
            }
            shots.push(ShotInfo { number, time_s, words: word_count(body) });
        }
        if let (true, false, Some(real)) = (times_ok, cuts_at.is_empty(), real) {
            let mut bounds = vec![0.0_f32];
            bounds.extend(&cuts_at);
            bounds.push(real);
            if let Some(pos) = bounds.windows(2).position(|w| w[1] - w[0] < 1.0) {
                let msg = format!(
                    "[Shot {}] lasts only {:.2} s; shots under 1 s rarely work",
                    pos + 1,
                    bounds[pos + 1] - bounds[pos]
                );
                self.info("short-shot", msg, None);
            }
        }
        if let Some(d) = duration {
            let cuts = shots.len().saturating_sub(1);
            let max = lengths::suggested_max_cuts(d) as usize;
            if cuts > max {
                let msg = format!("{cuts} cuts in a {d} s clip; about one cut per 3 s (up to {max}) works best");
                self.info("many-cuts", msg, None);
            }
        }
        shots
    }

    /// A one-take claim with several shots: `one take`, `single continuous shot` anywhere; `no cuts`,
    /// `without cutting` only where they speak about the whole video (the summary, the style text
    /// before the first shot), since inside a shot they describe that shot (`holds without cutting away`).
    fn single_take(&mut self, shot_count: usize, sections: &[Option<&Section>]) {
        if shot_count <= 1 {
            return;
        }
        for s in sections.iter().flatten() {
            let body = self.body(s);
            let is_description = s.name.ends_with("description");
            let whole_video = match self.re.shot.find(body) {
                Some(first) if is_description => &body[..first.start()],
                _ => body,
            };
            let claim = self.re.single_take.find(body).or_else(|| self.re.no_cuts.find(whole_video));
            if let Some(m) = claim {
                let msg = format!(
                    "\"{}\" but the description has {shot_count} shots: a one-take video uses only [Shot 1] (or remove the claim)",
                    m.as_str()
                );
                self.error("single-take-with-shots", msg, Some(s.start + m.start()));
                return;
            }
        }
    }

    fn word_limits(&mut self, words: usize, offset: usize, mode: Mode) {
        let (name, warn_below, info_below, info_above, target) = match mode {
            Mode::R2v => ("detailed_description", 200, 350, 600, "350-500"),
            Mode::F2v => ("integrated_multimodal_description", 100, 150, 400, "150-300"),
        };
        if words < warn_below {
            self.warn("description-short", format!("{name} has {words} words; aim for {target}"), Some(offset));
        } else if words < info_below {
            self.info(
                "description-below-target",
                format!("{name} has {words} words; the guide's target is {target}"),
                Some(offset),
            );
        } else if words > info_above {
            self.info(
                "description-long",
                format!("{name} has {words} words; the guide's target is {target}"),
                Some(offset),
            );
        }
    }

    fn sound_and_music(&mut self, sections: &[Section]) {
        let re = self.re;
        if let Some(s) = find_section(sections, "overall_soundscape") {
            let body = self.body(s);
            if body.contains("<d>") || re.speaker.is_match(body) {
                let msg = "overall_soundscape is for ambience and physical sounds: put speech in the shots";
                self.warn("dialogue-in-soundscape", msg, Some(s.header));
            }
            let n = sentence_count(body);
            if n > 4 {
                self.info("soundscape-long", format!("overall_soundscape has {n} sentences; use 1-4"), Some(s.header));
            }
        }
        if let Some(s) = find_section(sections, "non_diegetic_music") {
            let body = self.body(s).trim();
            let n = sentence_count(body);
            if !is_na(body) && n > 3 {
                self.info(
                    "music-long",
                    format!("non_diegetic_music has {n} sentences; use 1-3 (or N/A)"),
                    Some(s.header),
                );
            }
        }
    }

    /// `<d>` tags: closed, language tag, speaker id; quoted speech outside `<d>`.
    fn dialogue(&mut self) {
        let (text, re) = (self.text, self.re);
        let mut spans = Vec::new();
        let (mut search, mut prev_close) = (0, 0);
        let (mut no_lang, mut no_speaker) = (Vec::new(), Vec::new());
        let mut unclosed_reported = false;
        while let Some(rel) = text[search..].find("<d>") {
            let open = search + rel;
            let content = open + 3;
            let close = text[content..].find("</d>").map(|r| content + r);
            let next_open = text[content..].find("<d>").map(|r| content + r);
            let close = match (close, next_open) {
                (Some(c), Some(n)) if n < c => None,
                (c, _) => c,
            };
            let Some(close) = close else {
                if !unclosed_reported {
                    self.error("dialogue-unclosed", "a <d> dialogue tag is not closed with </d>", Some(open));
                    unclosed_reported = true;
                }
                search = content;
                continue;
            };
            if !re.lang_tag.is_match(&text[content..close]) {
                no_lang.push(open);
            }
            let window = floor_boundary(text, open.saturating_sub(200)).max(prev_close);
            if !re.speaker.is_match(&text[window..open]) {
                no_speaker.push(open);
            }
            spans.push((open, close + 4));
            prev_close = close + 4;
            search = prev_close;
        }
        if let Some(&first) = no_lang.first() {
            let msg = format!("{} dialogue line(s) without a language tag: write <d>[English] ...</d>", no_lang.len());
            self.warn("dialogue-language", msg, Some(first));
        }
        if let Some(&first) = no_speaker.first() {
            let msg = format!(
                "{} dialogue line(s) without a speaker id before them: write \"... (S1) says, <d>[English] ...</d>\"",
                no_speaker.len()
            );
            self.warn("dialogue-speaker", msg, Some(first));
        }
        // `a sign that says "OPEN"` is on-screen text, written in quotes as the guides ask.
        let outside = re
            .untagged_speech
            .find_iter(text)
            .find(|m| !spans.iter().any(|&(a, b)| m.start() >= a && m.start() < b) && !shows_text(text, m.start()));
        if let Some(m) = outside {
            let msg =
                format!("speech in quotes (\"{}\"): write it as (S1) says, <d>[English] ...</d>", snippet(m.as_str()));
            self.warn("dialogue-not-tagged", msg, Some(m.start()));
        }
    }

    fn no_text_line(&mut self) {
        if !self.text.to_lowercase().contains("no subtitles, captions, text, watermarks or logos") {
            let msg =
                format!("add \"{NO_TEXT_LINE}\" (this app's rule; r2v: after the style sentence, f2v: at the end of the description)");
            self.info("no-subtitles-line", msg, None);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Small helpers

/// A `[Shot N]` marker in a description (see `Linter::shot_markers`).
#[derive(Debug, Clone, Copy)]
struct ShotMarker {
    start: usize,
    end: usize,
    number: u32,
    /// Opens a shot (else it refers back to one).
    opens: bool,
}

/// The text before a `[Shot N]` marker lets it open a shot: nothing, a line break or a sentence
/// end (`.`, `!`, `?`, also before closing quotes or brackets, or `</d>`), markdown markers aside.
fn opens_shot(before: &str) -> bool {
    let before = before.trim_end_matches([' ', '\t']);
    if before.ends_with("</d>") {
        return true;
    }
    let before = before.trim_end_matches([' ', '\t', '*', '_', '-', '\u{2022}', '>', '#']);
    if before.is_empty() || before.ends_with('\n') {
        return true;
    }
    let before = before.trim_end_matches(['"', '\u{201d}', '\'', '\u{2019}', ')', ']']);
    before.ends_with(['.', '!', '?', '\u{2026}'])
}

/// Byte ranges of quoted text (`"..."`, `“...”`) and spoken lines (`<d>...</d>`).
fn quoted_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (open, close) in [("\"", "\""), ("\u{201c}", "\u{201d}"), ("<d>", "</d>")] {
        let mut from = 0;
        while let Some(a) = text[from..].find(open).map(|i| from + i) {
            let inner = a + open.len();
            let Some(b) = text[inner..].find(close).map(|i| inner + i) else { break };
            spans.push((a, b + close.len()));
            from = b + close.len();
        }
    }
    spans
}

/// Things that show text rather than speak it.
const TEXT_CARRIERS: [&str; 24] = [
    "sign",
    "signs",
    "signboard",
    "billboard",
    "banner",
    "poster",
    "placard",
    "label",
    "sticker",
    "screen",
    "display",
    "monitor",
    "note",
    "card",
    "page",
    "menu",
    "plaque",
    "neon",
    "caption",
    "headline",
    "logo",
    "notice",
    "t-shirt",
    "tattoo",
];

/// The speech verb at `at` belongs to something that shows text: the word before it (past `that`,
/// `which`, `it`) is a sign, screen, label ... (`a neon sign that says "OPEN"`, `her T-shirt says "NYC"`).
fn shows_text(text: &str, at: usize) -> bool {
    let mut words = text[..at].split_whitespace().rev();
    let word = loop {
        match words.next() {
            Some(w) if matches!(w.to_ascii_lowercase().as_str(), "that" | "which" | "it" | "also" | "clearly") => {}
            other => break other,
        }
    };
    word.is_some_and(|w| {
        let w = w.to_ascii_lowercase();
        let bare = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
        !w.ends_with([',', '.', ';', ':']) && TEXT_CARRIERS.contains(&bare)
    })
}

/// (offset of the match, number in group 1) for every match.
fn numbered_matches(re: &Regex, text: &str) -> Vec<(usize, u32)> {
    re.captures_iter(text).filter_map(|c| Some((c.get(0)?.start(), c.get(1)?.as_str().parse::<u32>().ok()?))).collect()
}

/// Words with at least one letter or digit.
fn word_count(text: &str) -> usize {
    text.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).count()
}

/// Sentences ending in `.`, `!` or `?` (a final unterminated one counts too).
fn sentence_count(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let (mut count, mut has_word) = (0, false);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_alphanumeric() {
            has_word = true;
        }
        let ends = matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if ends && has_word {
            count += 1;
            has_word = false;
        }
    }
    count + usize::from(has_word)
}

/// Byte index just after the first sentence of `text` (or its length).
fn first_sentence_end(text: &str) -> usize {
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(b, b'.' | b'!' | b'?') && bytes.get(i + 1).is_none_or(u8::is_ascii_whitespace) {
            return i + 1;
        }
    }
    text.len()
}

/// `00:05.000`, `00:05,250`, `5 s`, `3.5 seconds` -> seconds.
fn parse_loose_time(text: &str) -> Option<f32> {
    let text = text.trim();
    if text.contains(':') {
        lengths::parse_timestamp(&text.replace(',', "."))
    } else {
        text.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c.is_whitespace()).parse().ok()
    }
}

fn number_word(word: &str) -> Option<u32> {
    let n = match word.to_ascii_lowercase().as_str() {
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        other => return other.parse().ok(),
    };
    Some(n)
}

fn is_na(text: &str) -> bool {
    let t = text.trim().trim_end_matches('.');
    t.eq_ignore_ascii_case("n/a") || t.eq_ignore_ascii_case("none")
}

/// First 60 characters of `text`, trimmed.
fn snippet(text: &str) -> String {
    let text = text.trim();
    let mut out: String = text.chars().take(60).collect();
    if text.chars().count() > 60 {
        out.push_str("...");
    }
    out
}

/// Largest char boundary <= `index`.
fn floor_boundary(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const DEFS: &str = "subject_definitions:\n<Subject 1> is the woman in <Picture 1>, a young woman with long black hair.\n<Subject 2> is the cafe environment in <Picture 2>, with tall windows.";
    const SUMMARY: &str = "summary:\n[reference generation] The target video shows <Subject 1> sitting down in <Subject 2> in one continuous take.";
    const MULTI_SUMMARY: &str =
        "summary:\n[reference generation] The target video shows <Subject 1> sitting down in <Subject 2>, with cuts.";
    const RETENTION: &str = "retention_analysis:\n<Subject 1> (appears in [Shot 1]): fully_preserved - her face and hair are retained.\n<Subject 2> (appears in [Shot 1]): fully_preserved - the windows are retained.";
    const STYLE: &str =
        "The target video is a cinematic clip with a 35mm look. No subtitles, captions, text, watermarks or logos.";
    const SHOT1: &str = "[Shot 1] A medium shot frames <Subject 1>, the woman from <Picture 1>, in <Subject 2>. She walks to the window table and sits down. The camera pushes in with small amplitude at slow speed.";
    const SOUND: &str = "overall_soundscape: Quiet cafe room tone.";
    const MUSIC: &str = "non_diegetic_music: N/A";

    const F2V_DESC: &str = "integrated_multimodal_description: [Shot 1] Live-action, cinematic, a medium shot opens on <Picture 1>: a young woman in a white blouse stands at a cafe counter. She turns and walks to a window table, then sits down while the camera tracks with her at slow speed. No subtitles, captions, text, watermarks or logos.";
    const F2V_LAST: &str = " She sits, settling into the pose, spacing, light and composition established by Picture 2 at the end of the shot.";
    const F2V_TAIL: &str = "overall_soundscape: Quiet cafe room tone.\n\nnon_diegetic_music: N/A\n";

    fn r2v_with(defs: &str, summary: &str, retention: &str, shots: &str) -> String {
        format!("{defs}\n\n{summary}\n\n{retention}\n\ndetailed_description:\n{STYLE}\n{shots}\n\n{SOUND}\n\n{MUSIC}\n")
    }
    fn base_r2v() -> String {
        r2v_with(DEFS, SUMMARY, RETENTION, SHOT1)
    }
    fn f2v_with(line1: &str, desc: &str) -> String {
        format!("{line1}\n\n{desc}\n\n{F2V_TAIL}")
    }
    fn r2v_ctx(duration: f32, refs: usize, pinned: bool) -> LintContext {
        LintContext {
            mode: Mode::R2v,
            duration_s: Some(duration),
            ref_count: Some(refs),
            first_frame_pinned: Some(pinned),
            has_last_frame: None,
        }
    }
    fn f2v_ctx(duration: f32, refs: usize, last: bool) -> LintContext {
        LintContext {
            mode: Mode::F2v,
            duration_s: Some(duration),
            ref_count: Some(refs),
            first_frame_pinned: None,
            has_last_frame: Some(last),
        }
    }
    fn has(r: &LintReport, code: &str) -> bool {
        r.issues.iter().any(|i| i.code == code)
    }
    fn severity_of(r: &LintReport, code: &str) -> Option<Severity> {
        r.issues.iter().find(|i| i.code == code).map(|i| i.severity)
    }
    fn error_codes(r: &LintReport) -> BTreeSet<String> {
        r.issues.iter().filter(|i| i.severity == Severity::Error).map(|i| i.code.clone()).collect()
    }
    fn errs(r: &LintReport) -> Vec<&str> {
        r.issues.iter().filter(|i| i.severity == Severity::Error).map(|i| i.code.as_str()).collect()
    }
    fn fixtures_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
    }

    #[test]
    fn patterns_compile() {
        assert!(patterns().is_ok(), "{:?}", patterns().err());
    }

    #[test]
    fn fixtures_match_their_expectations() {
        let dir = fixtures_dir();
        let index = std::fs::read_to_string(dir.join("README.tsv")).expect("README.tsv");
        let mut checked = 0;
        for row in index.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let mut cols = row.split('\t');
            let file = cols.next().expect("file column");
            let mut ctx = LintContext::default();
            let mut expect: Option<BTreeSet<String>> = None;
            for col in cols {
                let (key, value) = col.split_once('=').expect("key=value");
                match key {
                    "mode" => ctx.mode = value.parse().expect("mode"),
                    "duration" => ctx.duration_s = Some(value.parse().expect("duration")),
                    "refs" => ctx.ref_count = Some(value.parse().expect("refs")),
                    "pinned" => ctx.first_frame_pinned = Some(value == "true"),
                    "last_frame" => ctx.has_last_frame = Some(value == "true"),
                    "expect" => {
                        let list = value.trim_start_matches("errors:");
                        expect = Some(
                            list.split(',').filter_map(|c| c.split_whitespace().next()).map(str::to_string).collect(),
                        );
                    }
                    other => panic!("unknown column {other} in README.tsv"),
                }
            }
            let text = std::fs::read_to_string(dir.join(file)).expect("fixture");
            let report = lint(&text, &ctx);
            let found = error_codes(&report);
            match expect {
                Some(want) => assert_eq!(found, want, "{file}: {:#?}", report.issues),
                None => {
                    assert!(file.starts_with("good_"), "{file} has no expectation");
                    assert!(found.is_empty(), "{file} should have no errors: {:#?}", report.issues);
                }
            }
            assert_eq!(report.ok, found.is_empty());
            assert_eq!(detect_mode(&text), Some(ctx.mode), "{file}");
            let infos = report.issues.iter().filter(|i| i.severity == Severity::Info).count();
            let penalty = 25 * report.errors() + 8 * report.warnings() + 2 * infos;
            assert_eq!(report.score as usize, 100usize.saturating_sub(penalty), "{file}");
            assert!(report.description_words > 0, "{file}");
            checked += 1;
        }
        assert!(checked >= 16, "only {checked} fixtures checked");
    }

    #[test]
    fn fixture_shots_and_times_are_parsed() {
        let dir = fixtures_dir();
        let text = std::fs::read_to_string(dir.join("good_r2v_romance_6.txt")).expect("fixture");
        let report = lint(&text, &r2v_ctx(15.0, 2, false));
        let times: Vec<Option<f32>> = report.shots.iter().map(|s| s.time_s).collect();
        assert_eq!(times, vec![None, Some(3.0), Some(6.0), Some(9.5), Some(12.5)]);
        assert_eq!(report.shots.iter().map(|s| s.number).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
        assert!(report.shots.iter().all(|s| s.words > 10));

        let bad = std::fs::read_to_string(dir.join("bad_r2v_markdown_fake_shots.txt")).expect("fixture");
        let report = lint(&bad, &r2v_ctx(15.0, 2, false));
        let times: Vec<Option<f32>> = report.shots.iter().map(|s| s.time_s).collect();
        assert_eq!(times, vec![None, Some(5.0), Some(10.0), Some(13.0)]);
        assert_eq!(report.issues.iter().filter(|i| i.code == "shot-time-format").count(), 3);
        assert_eq!(report.score, 0, "score is floored at 0");
        assert!(!report.ok);
    }

    #[test]
    fn clean_r2v_has_no_errors() {
        let report = lint(&base_r2v(), &r2v_ctx(6.0, 2, false));
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        assert!(report.ok);
        assert_eq!(report.shots.len(), 1);
        assert_eq!(report.shots[0].number, 1);
        assert_eq!(report.shots[0].time_s, None);
        // Only the (expected) short-description warning for this tiny example.
        let others: Vec<&str> =
            report.issues.iter().map(|i| i.code.as_str()).filter(|c| *c != "description-short").collect();
        assert!(others.is_empty(), "{others:?}");
        assert_eq!(report.score, 92);
        assert_eq!(report.summary(), "score 92 - 0 errors, 1 warnings");
    }

    #[test]
    fn markdown_headings_are_an_error_but_still_parsed() {
        let text = base_r2v()
            .replace("subject_definitions:", "**subject_definitions**")
            .replace("summary:\n", "### summary\n")
            .replace("overall_soundscape:", "**overall_soundscape:**");
        let report = lint(&text, &r2v_ctx(6.0, 2, false));
        assert_eq!(errs(&report), vec!["markdown-headings"], "{:#?}", report.issues);
        assert_eq!(report.issues.iter().filter(|i| i.code == "markdown-headings").count(), 1);
        assert_eq!(report.issues[0].line, Some(1));
        assert_eq!(report.shots.len(), 1);
    }

    #[test]
    fn missing_duplicate_empty_and_out_of_order_sections() {
        let ctx = r2v_ctx(6.0, 2, false);
        let missing = format!("{DEFS}\n\n{SUMMARY}\n\ndetailed_description:\n{STYLE}\n{SHOT1}\n\n{SOUND}\n\n{MUSIC}\n");
        let report = lint(&missing, &ctx);
        assert_eq!(errs(&report), vec!["missing-section"]);
        assert!(report.issues[0].message.contains("retention_analysis"));

        let swapped = r2v_with(SUMMARY, DEFS, RETENTION, SHOT1);
        assert!(has(&lint(&swapped, &ctx), "section-order"));

        let twice = format!("{}\n{SOUND}\n", base_r2v());
        assert!(has(&lint(&twice, &ctx), "duplicate-section"));

        let empty = r2v_with(DEFS, "summary:", RETENTION, SHOT1);
        let report = lint(&empty, &ctx);
        assert!(has(&report, "empty-section"));
        assert!(!has(&report, "missing-task-tag"));

        let lead_in = format!("Here is the prompt:\n\n{}", base_r2v());
        assert_eq!(severity_of(&lint(&lead_in, &ctx), "text-before-sections"), Some(Severity::Warning));

        let trailing = format!("{}\nLet me know if you want changes.\n", base_r2v());
        assert_eq!(severity_of(&lint(&trailing, &ctx), "text-after-sections"), Some(Severity::Warning));
    }

    #[test]
    fn task_tags_are_checked() {
        let ctx = r2v_ctx(6.0, 2, false);
        let none = SUMMARY.replace("[reference generation] ", "");
        assert_eq!(errs(&lint(&r2v_with(DEFS, &none, RETENTION, SHOT1), &ctx)), vec!["missing-task-tag"]);
        let video = SUMMARY.replace("[reference generation]", "[video editing]");
        assert_eq!(errs(&lint(&r2v_with(DEFS, &video, RETENTION, SHOT1), &ctx)), vec!["bad-task-tag"]);
        let repeated = SUMMARY.replace("[reference generation]", "[reference generation + reference generation]");
        assert_eq!(errs(&lint(&r2v_with(DEFS, &repeated, RETENTION, SHOT1), &ctx)), vec!["bad-task-tag"]);
        let unknown = SUMMARY.replace("[reference generation]", "[character animation]");
        assert!(has(&lint(&r2v_with(DEFS, &unknown, RETENTION, SHOT1), &ctx), "bad-task-tag"));
    }

    #[test]
    fn keyframe_wording_needs_a_pinned_frame() {
        let ctx = r2v_ctx(6.0, 2, false);
        let tag = SUMMARY.replace("[reference generation]", "[keyframe completion]");
        assert_eq!(errs(&lint(&r2v_with(DEFS, &tag, RETENTION, SHOT1), &ctx)), vec!["keyframe-without-frame"]);

        let frame_line = format!("{DEFS}\n<Picture 1> is the first frame of [Shot 1], showing her at the counter.");
        assert_eq!(
            errs(&lint(&r2v_with(&frame_line, SUMMARY, RETENTION, SHOT1), &ctx)),
            vec!["keyframe-without-frame"]
        );

        let begins =
            SHOT1.replace("[Shot 1] A medium shot", "[Shot 1] The shot begins from <Picture 1>: a medium shot");
        let report = lint(&r2v_with(DEFS, SUMMARY, RETENTION, &begins), &ctx);
        assert_eq!(errs(&report), vec!["keyframe-without-frame"]);
        assert_eq!(report.issues[0].line, Some(14)); // the [Shot 1] line
    }

    #[test]
    fn pinned_first_frame_rules() {
        let pinned = r2v_ctx(6.0, 2, true);
        // Pinned but written as a plain reference: warning only.
        let report = lint(&base_r2v(), &pinned);
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        assert_eq!(severity_of(&report, "pinned-frame-not-used"), Some(Severity::Warning));

        // Pinned and written as a real frame: clean.
        let defs = format!("{DEFS}\n<Picture 1> is the first frame of [Shot 1], showing her at the counter.");
        let summary = SUMMARY.replace("[reference generation]", "[reference generation + keyframe completion]");
        let retention = format!(
            "{RETENTION}\n<Picture 1> ([Shot 1] first frame): fully_preserved - the framing matches <Picture 1>."
        );
        let shot = SHOT1.replace("[Shot 1] A medium shot", "[Shot 1] The shot begins from <Picture 1>: a medium shot");
        let report = lint(&r2v_with(&defs, &summary, &retention, &shot), &pinned);
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        assert!(!has(&report, "pinned-frame-not-used"));
        assert!(!has(&report, "missing-retention"));

        // Only Ref 1 can be pinned.
        let wrong = format!("{DEFS}\n<Picture 2> is the first frame of [Shot 1], showing the cafe.");
        assert!(has(&lint(&r2v_with(&wrong, &summary, &retention, &shot), &pinned), "first-frame-picture"));

        // Unknown pin state: a lone keyframe tag only warns.
        let unknown = LintContext { first_frame_pinned: None, ..pinned };
        let tag = SUMMARY.replace("[reference generation]", "[keyframe completion]");
        let report = lint(&r2v_with(DEFS, &tag, RETENTION, SHOT1), &unknown);
        assert!(errs(&report).is_empty());
        assert!(has(&report, "keyframe-tag-unused"));
    }

    #[test]
    fn picture_numbers_are_checked() {
        let shot = format!("{SHOT1} A mirror shows <Picture 3>.");
        let report = lint(&r2v_with(DEFS, SUMMARY, RETENTION, &shot), &r2v_ctx(6.0, 2, false));
        assert_eq!(errs(&report), vec!["picture-out-of-range"]);
        assert!(report.issues[0].message.contains("only 2 picture(s)"));

        let unknown_count = LintContext { ref_count: None, ..r2v_ctx(6.0, 2, false) };
        let shot = format!("{SHOT1} A mirror shows <Picture 10>.");
        assert_eq!(
            errs(&lint(&r2v_with(DEFS, SUMMARY, RETENTION, &shot), &unknown_count)),
            vec!["picture-out-of-range"]
        );

        let report = lint(&base_r2v(), &r2v_ctx(6.0, 3, false));
        assert_eq!(severity_of(&report, "unused-picture"), Some(Severity::Warning));
    }

    #[test]
    fn subjects_must_be_defined() {
        let ctx = r2v_ctx(6.0, 2, false);
        let shot = format!("{SHOT1} <Subject 3> waves.");
        let report = lint(&r2v_with(DEFS, SUMMARY, RETENTION, &shot), &ctx);
        assert_eq!(errs(&report), vec!["undefined-subject"]);
        assert!(report.issues[0].message.contains("<Subject 3>"));

        let no_labels = "subject_definitions:\nThe woman from Picture 1 and the cafe from Picture 2.";
        assert!(has(&lint(&r2v_with(no_labels, SUMMARY, RETENTION, SHOT1), &ctx), "no-subjects"));

        let unused = SHOT1.replace(" in <Subject 2>", "");
        assert_eq!(
            severity_of(&lint(&r2v_with(DEFS, SUMMARY, RETENTION, &unused), &ctx), "unused-subject"),
            Some(Severity::Info)
        );
    }

    #[test]
    fn retention_rules() {
        let ctx = r2v_ctx(6.0, 2, false);
        let bad_marker = RETENTION.replace("fully_preserved - her", "fully preserved - her");
        assert_eq!(errs(&lint(&r2v_with(DEFS, SUMMARY, &bad_marker, SHOT1), &ctx)), vec!["bad-retention-marker"]);
        let no_marker =
            RETENTION.replace("fully_preserved - her face and hair are retained", "her face and hair are retained");
        assert!(has(&lint(&r2v_with(DEFS, SUMMARY, &no_marker, SHOT1), &ctx), "bad-retention-marker"));
        let speaker = RETENTION.replace("her face and hair", "her (S1) face and hair");
        assert_eq!(errs(&lint(&r2v_with(DEFS, SUMMARY, &speaker, SHOT1), &ctx)), vec!["speaker-id-in-retention"]);
        let no_parens = RETENTION.replace("<Subject 1> (appears in [Shot 1]):", "<Subject 1>:");
        let report = lint(&r2v_with(DEFS, SUMMARY, &no_parens, SHOT1), &ctx);
        assert!(errs(&report).is_empty());
        assert_eq!(severity_of(&report, "retention-format"), Some(Severity::Warning));
        let one_line =
            "retention_analysis:\n<Subject 1> (appears in [Shot 1]): weak_reference - only her look is kept.";
        assert_eq!(
            severity_of(&lint(&r2v_with(DEFS, SUMMARY, one_line, SHOT1), &ctx), "missing-retention"),
            Some(Severity::Warning)
        );
    }

    #[test]
    fn shot_one_must_not_have_a_time() {
        let shot = SHOT1.replace("[Shot 1] A medium", "[Shot 1] At 00:00.000, a medium");
        let report = lint(&r2v_with(DEFS, SUMMARY, RETENTION, &shot), &r2v_ctx(6.0, 2, false));
        assert_eq!(errs(&report), vec!["shot1-time"]);
        assert!(!has(&report, "time-inside-shot"));
    }

    fn two_shots(second: &str) -> String {
        let shots = format!("{SHOT1}\n[Shot 2] {second} the camera cuts to a close-up of <Subject 1>. She smiles.");
        r2v_with(DEFS, MULTI_SUMMARY, RETENTION, &shots)
    }

    #[test]
    fn later_shots_need_at_timestamps() {
        let ctx = r2v_ctx(6.0, 2, false);
        let ok = lint(&two_shots("At 00:03.000,"), &ctx);
        assert!(errs(&ok).is_empty(), "{:#?}", ok.issues);
        assert_eq!(ok.shots.iter().map(|s| s.time_s).collect::<Vec<_>>(), vec![None, Some(3.0)]);
        assert!(!has(&ok, "cut-without-shot"));

        let around = lint(&two_shots("Around 00:03.000"), &ctx);
        assert_eq!(errs(&around), vec!["shot-time-format"]);
        assert!(around.issues[0].message.contains("At 00:03.000,"));
        assert_eq!(around.shots[1].time_s, Some(3.0));

        let no_comma = lint(&two_shots("At 00:03.000"), &ctx);
        assert_eq!(errs(&no_comma), vec!["shot-time-format"]);
        let seconds = lint(&two_shots("At 3 seconds,"), &ctx);
        assert_eq!(errs(&seconds), vec!["shot-time-format"]);
        assert_eq!(seconds.shots[1].time_s, Some(3.0));
        let missing = lint(&two_shots("Then"), &ctx);
        assert_eq!(errs(&missing), vec!["shot-time-missing"]);
        assert_eq!(missing.shots[1].time_s, None);
    }

    #[test]
    fn shot_times_increase_and_stay_inside_the_clip() {
        let ctx = r2v_ctx(6.0, 2, false);
        let three = format!(
            "{SHOT1}\n[Shot 2] At 00:04.000, the camera cuts to <Subject 1> at the table.\n[Shot 3] At 00:03.000, the camera cuts to her face."
        );
        assert_eq!(errs(&lint(&r2v_with(DEFS, MULTI_SUMMARY, RETENTION, &three), &ctx)), vec!["shot-time-order"]);
        assert_eq!(errs(&lint(&two_shots("At 00:07.000,"), &ctx)), vec!["shot-time-past-end"]);
        assert_eq!(errs(&lint(&two_shots("At 00:06.600,"), &ctx)), vec!["shot-time-past-end"]);
        // 6 s is really 6.58 s, so 6.5 s is still inside the clip (but a very short last shot).
        let late = lint(&two_shots("At 00:06.500,"), &ctx);
        assert!(errs(&late).is_empty(), "{:#?}", late.issues);
        assert_eq!(severity_of(&late, "short-shot"), Some(Severity::Info));
        // Unknown duration: no end check.
        let free = LintContext { duration_s: None, ..ctx.clone() };
        assert!(errs(&lint(&two_shots("At 00:30.000,"), &free)).is_empty());
        let renumbered = two_shots("At 00:03.000,").replace("[Shot 2]", "[Shot 3]");
        assert!(has(&lint(&renumbered, &ctx), "shot-numbering"));
    }

    #[test]
    fn times_and_cuts_inside_a_shot_warn() {
        let ctx = r2v_ctx(6.0, 2, false);
        let timed = format!("{SHOT1} At 00:01.200, she smiles.");
        let report = lint(&r2v_with(DEFS, SUMMARY, RETENTION, &timed), &ctx);
        assert!(errs(&report).is_empty());
        assert_eq!(severity_of(&report, "time-inside-shot"), Some(Severity::Warning));
        let cut = format!("{SHOT1} Then the camera cuts to a close-up of her face.");
        let report = lint(&r2v_with(DEFS, SUMMARY, RETENTION, &cut), &ctx);
        assert_eq!(severity_of(&report, "cut-without-shot"), Some(Severity::Warning));
    }

    #[test]
    fn single_take_claims_with_several_shots_are_errors() {
        let ctx = r2v_ctx(6.0, 2, false);
        let shots = format!("{SHOT1}\n[Shot 2] At 00:03.000, the camera cuts to a close-up of <Subject 1>.");
        let summaries = [
            SUMMARY.to_string(),
            MULTI_SUMMARY.replace("with cuts", "with no cuts"),
            MULTI_SUMMARY.replace("with cuts", "in a single continuous take"),
        ];
        for summary in summaries {
            let report = lint(&r2v_with(DEFS, &summary, RETENTION, &shots), &ctx);
            assert_eq!(errs(&report), vec!["single-take-with-shots"], "{summary}");
        }
        let report = lint(&r2v_with(DEFS, SUMMARY, RETENTION, SHOT1), &ctx);
        assert!(!has(&report, "single-take-with-shots"));
        // "Without cutting" inside a shot describes that shot, not the whole video.
        let held = shots.replace("She walks", "The camera holds a static shot without cutting away. She walks");
        let report = lint(&r2v_with(DEFS, MULTI_SUMMARY, RETENTION, &held), &ctx);
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        let style = format!("{STYLE} It plays with no cuts.");
        let claimed = r2v_with(DEFS, MULTI_SUMMARY, RETENTION, &shots).replace(STYLE, &style);
        assert_eq!(errs(&lint(&claimed, &ctx)), vec!["single-take-with-shots"]);
    }

    #[test]
    fn shot_references_inside_the_description_are_not_new_shots() {
        let ctx = r2v_ctx(3.0, 2, false);
        let base = std::fs::read_to_string(fixtures_dir().join("good_r2v_e2.txt")).expect("fixture");
        let referenced =
            base.replace("in her white blouse,", "in her white blouse, framed like the opening of [Shot 1],");
        assert_ne!(referenced, base);
        let report = lint(&referenced, &ctx);
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        assert_eq!(report.shots.iter().map(|s| (s.number, s.time_s)).collect::<Vec<_>>(), [(1, None), (2, Some(1.5))]);
        assert!(!has(&report, "shot-count-mismatch") && !has(&report, "many-cuts"));
        // A reference to a shot that does not exist is reported.
        let report = lint(&base.replace("in her white blouse,", "in her white blouse, as in [Shot 4],"), &ctx);
        assert_eq!(severity_of(&report, "unknown-shot"), Some(Severity::Warning));
        // A shot opened after a sentence end on the same line still counts.
        let inline = base.replace("Hey, you made it.</d>\n[Shot 2]", "Hey, you made it.</d> [Shot 2]");
        assert_ne!(inline, base);
        assert_eq!(lint(&inline, &ctx).shots.len(), 2);
        let inline = base.replace(" (S1) says: <d>[English] Hey, you made it.</d>\n[Shot 2]", " waves.  [Shot 2]");
        assert_eq!(lint(&inline, &ctx).shots.len(), 2);
    }

    #[test]
    fn quoted_on_screen_text_is_not_speech_or_a_time() {
        let ctx = r2v_ctx(6.0, 2, false);
        let with = |line: &str| lint(&r2v_with(DEFS, SUMMARY, RETENTION, &format!("{SHOT1} {line}")), &ctx);
        let sign = with("Behind her hangs a sign that says \"NO DOGS\", and her T-shirt says \"NYC\".");
        assert!(!has(&sign, "dialogue-not-tagged"), "{:#?}", sign.issues);
        let clock = with("A park clock reading \"4:15\" hangs above the door.");
        assert!(!has(&clock, "time-inside-shot"), "{:#?}", clock.issues);
        let spoken = with("<Subject 1> (S1) says, <d>[English] Meet me at 4:15.</d>");
        assert!(!has(&spoken, "time-inside-shot") && !has(&spoken, "dialogue-not-tagged"), "{:#?}", spoken.issues);
        // People still have to be tagged.
        let talk = with("She looks at the sign and says \"Wow.\"");
        assert_eq!(severity_of(&talk, "dialogue-not-tagged"), Some(Severity::Warning));
        assert_eq!(severity_of(&with("At 00:02.000 she smiles."), "time-inside-shot"), Some(Severity::Warning));
    }

    #[test]
    fn summary_shot_count_and_unknown_shots_warn() {
        let ctx = r2v_ctx(6.0, 2, false);
        let shots = format!("{SHOT1}\n[Shot 2] At 00:03.000, the camera cuts to a close-up of <Subject 1>.");
        let summary = MULTI_SUMMARY.replace("with cuts", "in three shots");
        let report = lint(&r2v_with(DEFS, &summary, RETENTION, &shots), &ctx);
        assert_eq!(severity_of(&report, "shot-count-mismatch"), Some(Severity::Warning));
        let summary = MULTI_SUMMARY.replace("with cuts", "in two shots");
        assert!(!has(&lint(&r2v_with(DEFS, &summary, RETENTION, &shots), &ctx), "shot-count-mismatch"));
        let retention =
            RETENTION.replace("<Subject 1> (appears in [Shot 1])", "<Subject 1> (appears in [Shot 1], [Shot 3])");
        let report = lint(&r2v_with(DEFS, MULTI_SUMMARY, &retention, &shots), &ctx);
        assert_eq!(severity_of(&report, "unknown-shot"), Some(Severity::Warning));
    }

    #[test]
    fn description_word_limits() {
        let ctx = r2v_ctx(6.0, 2, false);
        let with_words = |n: usize| {
            let shot = format!("[Shot 1] <Subject 1> and <Subject 2> {}", "word ".repeat(n));
            lint(&r2v_with(DEFS, SUMMARY, RETENTION, &shot), &ctx)
        };
        assert_eq!(severity_of(&with_words(50), "description-short"), Some(Severity::Warning));
        let mid = with_words(250);
        assert_eq!(severity_of(&mid, "description-below-target"), Some(Severity::Info));
        assert!(!has(&mid, "description-short"));
        let good = with_words(400);
        assert!(
            !has(&good, "description-short")
                && !has(&good, "description-below-target")
                && !has(&good, "description-long")
        );
        assert!(good.description_words > 400 && good.description_words < 450, "{}", good.description_words);
        assert_eq!(severity_of(&with_words(700), "description-long"), Some(Severity::Info));
    }

    #[test]
    fn no_subtitles_line_and_style_sentence() {
        let ctx = r2v_ctx(6.0, 2, false);
        let text = base_r2v().replace(" No subtitles, captions, text, watermarks or logos.", "");
        assert_eq!(severity_of(&lint(&text, &ctx), "no-subtitles-line"), Some(Severity::Info));
        assert!(!has(&lint(&base_r2v(), &ctx), "no-subtitles-line"));
        let no_style = base_r2v().replace(&format!("{STYLE}\n"), "");
        assert!(has(&lint(&no_style, &ctx), "missing-style-sentence"));
        let odd_style = base_r2v().replace("The target video is a cinematic", "A cinematic");
        assert_eq!(severity_of(&lint(&odd_style, &ctx), "style-sentence-start"), Some(Severity::Info));
    }

    #[test]
    fn soundscape_and_music_rules() {
        let ctx = r2v_ctx(6.0, 2, false);
        let talk = base_r2v().replace(SOUND, "overall_soundscape: Room tone. She (S1) says, <d>[English] Hi.</d>");
        assert_eq!(severity_of(&lint(&talk, &ctx), "dialogue-in-soundscape"), Some(Severity::Warning));
        let long_music = base_r2v().replace(MUSIC, "non_diegetic_music: Piano. Strings. Drums. Bass.");
        assert_eq!(severity_of(&lint(&long_music, &ctx), "music-long"), Some(Severity::Info));
        let long_sound = base_r2v().replace(SOUND, "overall_soundscape: Rain. Wind. Steps. Doors. Birds.");
        assert_eq!(severity_of(&lint(&long_sound, &ctx), "soundscape-long"), Some(Severity::Info));
    }

    #[test]
    fn dialogue_rules() {
        let ctx = r2v_ctx(6.0, 2, false);
        let with = |line: &str| lint(&r2v_with(DEFS, SUMMARY, RETENTION, &format!("{SHOT1} {line}")), &ctx);
        let good = with("<Subject 1> (S1) says, <d>[English] Good morning.</d>");
        assert!(
            !has(&good, "dialogue-language") && !has(&good, "dialogue-speaker") && !has(&good, "dialogue-not-tagged")
        );
        assert_eq!(
            severity_of(&with("<Subject 1> (S1) says, <d>Good morning.</d>"), "dialogue-language"),
            Some(Severity::Warning)
        );
        assert_eq!(
            severity_of(&with("She says, <d>[English] Good morning.</d>"), "dialogue-speaker"),
            Some(Severity::Warning)
        );
        assert_eq!(errs(&with("<Subject 1> (S1) says, <d>[English] Good morning.")), vec!["dialogue-unclosed"]);
        assert_eq!(severity_of(&with("She says \"Good morning.\""), "dialogue-not-tagged"), Some(Severity::Warning));
    }

    #[test]
    fn clean_f2v_i2va_and_fl2va() {
        let report = lint(&f2v_with(I2VA_LINE, F2V_DESC), &f2v_ctx(6.0, 1, false));
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        assert_eq!(report.mode, Mode::F2v);
        assert_eq!(report.shots.len(), 1);

        let desc = format!("{F2V_DESC}{F2V_LAST}");
        let report = lint(&f2v_with(&fl2va_line(6.0, 1), &desc), &f2v_ctx(6.0, 2, true));
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        assert!(!has(&report, "fl2va-ending"));
        assert!(fl2va_line(6.0, 1).contains("the 6.58-second mark"));
    }

    #[test]
    fn f2v_line1_must_match_the_last_frame() {
        let report = lint(&f2v_with(I2VA_LINE, &format!("{F2V_DESC}{F2V_LAST}")), &f2v_ctx(6.0, 2, true));
        assert_eq!(errs(&report), vec!["f2v-line1-mismatch"]);
        assert!(report.issues[0].message.contains("6.58-second mark"));
        let report = lint(&f2v_with(&fl2va_line(6.0, 1), F2V_DESC), &f2v_ctx(6.0, 1, false));
        assert!(error_codes(&report).contains("f2v-line1-mismatch"), "{:#?}", report.issues);
        let ending = lint(&f2v_with(&fl2va_line(6.0, 1), F2V_DESC), &f2v_ctx(6.0, 2, true));
        assert_eq!(severity_of(&ending, "fl2va-ending"), Some(Severity::Warning));
    }

    #[test]
    fn fl2va_seconds_must_be_the_real_length() {
        let desc = format!("{F2V_DESC}{F2V_LAST}");
        let wrong = fl2va_line(6.0, 1).replace("6.58", "6.00");
        let report = lint(&f2v_with(&wrong, &desc), &f2v_ctx(6.0, 2, true));
        assert_eq!(errs(&report), vec!["fl2va-length"]);
        assert!(report.issues[0].message.contains("6.58"));
        assert_eq!(report.issues[0].line, Some(1));
        let ten = lint(&f2v_with(&fl2va_line(10.0, 1), &desc), &f2v_ctx(10.0, 2, true));
        assert!(errs(&ten).is_empty(), "{:#?}", ten.issues);
        assert!(fl2va_line(10.0, 1).contains("10.13-second"));
    }

    #[test]
    fn fl2va_shot_must_be_the_last_shot() {
        let desc = format!(
            "{F2V_DESC}\n[Shot 2] At 00:03.000, the camera cuts to her face as she sits, settling into the composition of Picture 2."
        );
        let report = lint(&f2v_with(&fl2va_line(6.0, 1), &desc), &f2v_ctx(6.0, 2, true));
        assert_eq!(errs(&report), vec!["fl2va-shot"]);
        let report = lint(&f2v_with(&fl2va_line(6.0, 2), &desc), &f2v_ctx(6.0, 2, true));
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        // The suggested FL2VA line names the last shot, so pasting it in fixes the prompt.
        let report = lint(&f2v_with(I2VA_LINE, &desc), &f2v_ctx(6.0, 2, true));
        assert_eq!(errs(&report), vec!["f2v-line1-mismatch"]);
        assert!(report.issues[0].message.ends_with(&fl2va_line(6.0, 2)), "{}", report.issues[0].message);
    }

    #[test]
    fn f2v_lead_in_is_reported_instead_of_line_1() {
        let ctx = f2v_ctx(6.0, 1, false);
        let text = format!("Here is the prompt:\n\n{}", f2v_with(I2VA_LINE, F2V_DESC));
        let report = lint(&text, &ctx);
        assert_eq!(errs(&report), vec!["f2v-line1-missing"]);
        let before: Vec<&str> =
            report.issues.iter().filter(|i| i.code == "text-before-sections").map(|i| i.message.as_str()).collect();
        assert_eq!(before.len(), 1);
        assert!(before[0].contains("Here is the prompt"), "{}", before[0]);
        assert!(!has(&lint(&f2v_with(I2VA_LINE, F2V_DESC), &ctx), "text-before-sections"));
    }

    #[test]
    fn f2v_line1_missing_malformed_dash_and_blank() {
        let ctx = f2v_ctx(6.0, 1, false);
        let missing = format!("{F2V_DESC}\n\n{F2V_TAIL}");
        assert_eq!(errs(&lint(&missing, &ctx)), vec!["f2v-line1-missing"]);
        let malformed = f2v_with("For the target video, at 0.00 seconds, <Picture 1> is referenced.", F2V_DESC);
        assert_eq!(errs(&lint(&malformed, &ctx)), vec!["f2v-line1-format"]);
        let desc = format!("{F2V_DESC}{F2V_LAST}");
        let hyphen = fl2va_line(6.0, 1).replace('—', "-");
        let report = lint(&f2v_with(&hyphen, &desc), &f2v_ctx(6.0, 2, true));
        assert!(errs(&report).is_empty(), "{:#?}", report.issues);
        assert_eq!(severity_of(&report, "f2v-line1-dash"), Some(Severity::Info));
        let squashed = format!("{I2VA_LINE}\n{F2V_DESC}\n\n{F2V_TAIL}");
        assert_eq!(severity_of(&lint(&squashed, &ctx), "f2v-line1-blank"), Some(Severity::Warning));
    }

    #[test]
    fn f2v_rejects_r2v_words_and_checks_field_order() {
        let ctx = f2v_ctx(6.0, 1, false);
        let subject = f2v_with(I2VA_LINE, &format!("{F2V_DESC} <Subject 1> smiles."));
        assert_eq!(errs(&lint(&subject, &ctx)), vec!["r2v-words-in-f2v"]);
        let summary = format!("{I2VA_LINE}\n\nsummary: a woman sits.\n\n{F2V_DESC}\n\n{F2V_TAIL}");
        assert!(has(&lint(&summary, &ctx), "r2v-words-in-f2v"));
        let order = format!("{I2VA_LINE}\n\noverall_soundscape: Quiet.\n\n{F2V_DESC}\n\nnon_diegetic_music: N/A\n");
        assert_eq!(errs(&lint(&order, &ctx)), vec!["section-order"]);
        let no_music = format!("{I2VA_LINE}\n\n{F2V_DESC}\n\noverall_soundscape: Quiet.\n");
        assert_eq!(errs(&lint(&no_music, &ctx)), vec!["missing-section"]);
        let picture2 = f2v_with(I2VA_LINE, &format!("{F2V_DESC} Picture 2 is not used."));
        assert_eq!(errs(&lint(&picture2, &ctx)), vec!["picture-out-of-range"]);
        let no_shot = f2v_with(I2VA_LINE, &F2V_DESC.replace("[Shot 1] ", ""));
        assert_eq!(errs(&lint(&no_shot, &ctx)), vec!["missing-shot-1"]);
    }

    #[test]
    fn wrong_mode_and_empty_prompt() {
        let f2v_text = f2v_with(I2VA_LINE, F2V_DESC);
        let report = lint(&f2v_text, &r2v_ctx(6.0, 1, false));
        assert_eq!(errs(&report), vec!["wrong-mode"]);
        assert_eq!(report.issues.len(), 1);
        let report = lint(&base_r2v(), &f2v_ctx(6.0, 1, false));
        assert_eq!(errs(&report), vec!["wrong-mode"]);

        let empty = lint("   \n", &LintContext::default());
        assert_eq!(errs(&empty), vec!["empty-prompt"]);
        assert_eq!(empty.score, 75);
        assert!(!empty.ok);
    }

    #[test]
    fn detect_mode_cases() {
        assert_eq!(detect_mode(&base_r2v()), Some(Mode::R2v));
        assert_eq!(detect_mode("**subject_definitions**\n<Subject 1> is ..."), Some(Mode::R2v));
        assert_eq!(detect_mode(&f2v_with(I2VA_LINE, F2V_DESC)), Some(Mode::F2v));
        assert_eq!(detect_mode(F2V_DESC), Some(Mode::F2v));
        assert_eq!(detect_mode(&format!("\n  {}\n", fl2va_line(6.0, 1))), Some(Mode::F2v));
        assert_eq!(
            detect_mode("For the target video, at 0.00 seconds into the target video, <Picture 1> ..."),
            Some(Mode::F2v)
        );
        assert_eq!(detect_mode("A woman walks into a cafe."), None);
        assert_eq!(detect_mode(""), None);
        // The alignment line decides: a stray r2v field name in an f2v answer does not flip it.
        let stray =
            f2v_with(I2VA_LINE, &F2V_DESC.replace("integrated_multimodal_description:", "detailed_description:"));
        assert_eq!(detect_mode(&stray), Some(Mode::F2v));
        assert!(has(&lint(&stray, &f2v_ctx(6.0, 1, false)), "r2v-words-in-f2v"));
        let mentioned = f2v_with(I2VA_LINE, &format!("{F2V_DESC} Her detailed_description stays the same."));
        assert_eq!(detect_mode(&mentioned), Some(Mode::F2v));
        assert_eq!(detect_mode(&format!("{}\n\n{I2VA_LINE}", base_r2v())), Some(Mode::R2v));
    }

    #[test]
    fn loose_timestamps_and_counters() {
        assert_eq!(parse_loose_time("00:05.000"), Some(5.0));
        assert_eq!(parse_loose_time("00:05,250"), Some(5.25));
        assert_eq!(parse_loose_time("01:02.500"), Some(62.5));
        assert_eq!(parse_loose_time("3.5 s"), Some(3.5));
        assert_eq!(parse_loose_time("4 seconds"), Some(4.0));
        assert_eq!(parse_loose_time("soon"), None);
        assert_eq!(sentence_count("N/A"), 1);
        assert_eq!(sentence_count("One. Two! Three? Four"), 4);
        assert_eq!(word_count("<Subject 1> walks - fast."), 4);
        assert_eq!(number_word("Three"), Some(3));
        assert_eq!(number_word("12"), Some(12));
    }
}
