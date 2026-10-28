//! Describe a reference picture once with Bonsai's vision input, so prompts are written from text.
//!
//! Sending a picture with every prompt costs ~1,100 prompt tokens and ~18 s each on the laptop,
//! and the picture tokens are never reused from llama-server's cache across different requests.
//! Instead Bonsai looks at each picture **once** for its role ([`RefRole`]) and writes one compact
//! paragraph of visual facts; that text goes into the reference's description and the prompt
//! request stays text-only (fast and cache-friendly).
//!
//! - [`messages`] builds the request: a fixed system prompt (byte-identical every time, so
//!   llama-server keeps it cached), the role's instruction and the picture as an `image_url` data
//!   URI ([`ImageAttachment::from_file`] scales big pictures down to 1024 px).
//! - [`params`]: thinking off, temperature 0.2, at most [`MAX_TOKENS`] new tokens.
//! - [`clean_description`] removes quotes, markdown and lead-ins such as "This image shows".
//! - [`DescriptionCache`]: `config_dir()\descriptions\<sha256 of the file>-<role>.json`, so the
//!   same picture and role is never described twice (a re-describe skips the cache).
//! - A server without a vision projector cannot do it: [`check_vision`] gives [`NO_VISION`].
//!
//! The GUI runs it as `service::JobKind::Describe`; the CLI as `bh3 describe`; MCP as
//! `describe_image`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::{ChatResult, ServerProps, Timings};
use crate::gen::{ChatMessage, GenParams, ImageAttachment, ThinkingMode};
use crate::request::RefRole;

/// Most new tokens of a description (a 40-90 word paragraph is ~60-130 tokens).
pub const MAX_TOKENS: u32 = 220;
/// Sampling temperature: low, the description should say what is there, not invent.
pub const TEMPERATURE: f32 = 0.2;
/// Folder of the cache under the config folder.
pub const CACHE_DIR: &str = "descriptions";
/// The error when the server has no vision projector (mmproj).
pub const NO_VISION: &str =
    "this Bonsai has no vision projector (mmproj), so it cannot see pictures - describe the picture in words";

/// The system prompt of every describe request (kept byte-identical for llama-server's cache).
pub const SYSTEM_PROMPT: &str = "You describe one reference picture for a video prompt writer. Answer with ONE \
compact paragraph of plain text, 40 to 90 words: no markdown, no lists, no headings, no quotes, and no lead-in such \
as \"This image shows\". Start straight with the subject as a noun phrase, for example \"a woman in her late \
twenties with ...\" or \"a narrow cobblestone street ...\". Say only what is visible, as concrete visual facts: \
colours, materials, shapes, light, positions. Never guess names, identities, brands or a story, never judge \
attractiveness or beauty, and give no advice.";

/// What Bonsai looks at for a role (the user message next to the picture).
pub fn instruction(role: RefRole) -> &'static str {
    match role {
        RefRole::Person | RefRole::SamePerson => {
            "Describe the person: apparent age range, face, hair (colour, length, style), skin tone, build, the \
             clothing with its colours and materials, accessories, and the pose and expression. Leave out the \
             background."
        }
        RefRole::Place => {
            "Describe the place only: the kind of setting, the light and time of day, the key objects and \
             surfaces, the colours and the mood. Leave out any people."
        }
        RefRole::PersonAndPlace => {
            "Describe the person (apparent age range, hair, skin tone, build, clothing with colours and materials, \
             pose), then the place around them (setting, light and time of day, key objects, colours)."
        }
        RefRole::Outfit => {
            "Describe only the outfit: each garment with its cut, colours, materials and details, plus shoes and \
             accessories. Do not describe the person wearing it or the background."
        }
        RefRole::Prop => {
            "Describe only the object: what it is, its shape and size, colours, materials and details. Leave out \
             the background and any people."
        }
        RefRole::Style => {
            "Describe only the look, not the content: the colour grade and palette, the light (quality, \
             direction, contrast), the lens and depth of field, grain or texture, and the overall style (for \
             example 35mm film, glossy commercial, 2D anime). Do not describe the people or objects."
        }
        RefRole::FirstFrame | RefRole::LastFrame | RefRole::Keyframe => {
            "Describe exactly what this frame shows: who is in it (apparent age range, hair, clothing), their pose \
             and position in the frame, the framing (close-up, medium or wide shot), the camera angle and height, \
             and the setting with its light."
        }
        RefRole::Storyboard => {
            "Describe the storyboard: how many panels, and for each panel in order its framing, subject and \
             action, in one paragraph."
        }
    }
}

/// Generation settings of a describe request: thinking off, temperature 0.2, [`MAX_TOKENS`].
pub fn params() -> GenParams {
    GenParams {
        thinking: ThinkingMode::Off,
        temperature: TEMPERATURE,
        top_p: 0.9,
        top_k: 20,
        min_p: 0.05,
        max_tokens: MAX_TOKENS,
        seed: None,
        stream: true,
    }
}

/// The request for one picture: the system prompt, then the role's instruction with the picture.
pub fn messages(image: ImageAttachment, role: RefRole) -> Vec<ChatMessage> {
    let user = format!("{}\nThe picture is used as: {}.", instruction(role), role.label().to_ascii_lowercase());
    vec![ChatMessage::system(SYSTEM_PROMPT), ChatMessage::user(user).with_images(vec![image])]
}

/// `Err(NO_VISION)` when the server has no vision projector.
pub fn check_vision(props: &ServerProps) -> anyhow::Result<()> {
    if props.vision {
        Ok(())
    } else {
        bail!("{NO_VISION}")
    }
}

/// Lead-ins Bonsai may still start with (compared in lower case, longest first per start).
const LEAD_INS: &[&str] = &[
    "here is a description of the picture:",
    "here is a description of the image:",
    "here is the description:",
    "description:",
    "this image shows",
    "this image depicts",
    "this picture shows",
    "this picture depicts",
    "this photo shows",
    "this photograph shows",
    "this frame shows",
    "the image shows",
    "the image depicts",
    "the picture shows",
    "the picture depicts",
    "the photo shows",
    "the photograph shows",
    "the frame shows",
    "in this image,",
    "in this picture,",
    "in this photo,",
    "in the image,",
    "in the picture,",
    "this is a picture of",
    "this is a photo of",
    "this is an image of",
    "the image is",
    "the picture is",
];

/// One plain paragraph from Bonsai's answer: no code fences, headings, list marks, bold, quotes or
/// "This image shows" lead-ins; whitespace collapsed; a leading article in lower case ("a woman ...",
/// ready for `Ref 1: a woman ... (person).`).
pub fn clean_description(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("```")).collect();
    // Heading lines ("# Person") are titles, not description - unless there is nothing else.
    let only_headings = lines.iter().all(|l| l.starts_with('#'));
    let mut parts: Vec<String> = Vec::new();
    for line in lines {
        if line.starts_with('#') && !only_headings {
            continue;
        }
        let mut line = line.trim_start_matches('#').trim_start();
        line = strip_list_mark(line);
        if !line.is_empty() {
            parts.push(line.to_string());
        }
    }
    let mut out = parts.join(" ").replace("**", "").replace("__", "").replace('`', "");
    out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let before = out.len();
        out = strip_quotes(&out).to_string();
        out = strip_lead_in(&out).to_string();
        if out.len() == before {
            break;
        }
    }
    lower_article(out.trim())
}

/// `- `, `* `, `• `, `1. `, `1) ` at the start of a line.
fn strip_list_mark(line: &str) -> &str {
    for mark in ["- ", "* ", "• ", "+ "] {
        if let Some(rest) = line.strip_prefix(mark) {
            return rest.trim_start();
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 2 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

/// Quotes around the whole text.
fn strip_quotes(text: &str) -> &str {
    let text = text.trim();
    for (open, close) in [('"', '"'), ('\u{201c}', '\u{201d}'), ('\'', '\''), ('\u{2018}', '\u{2019}')] {
        if let Some(inner) = text.strip_prefix(open).and_then(|t| t.strip_suffix(close)) {
            return inner.trim();
        }
    }
    // A quote only at the start (the answer was cut) or only at the end.
    text.trim_start_matches(['"', '\u{201c}']).trim_end_matches(['"', '\u{201d}']).trim()
}

/// `text` without a leading "This image shows" (and a following "a picture of" / ":").
fn strip_lead_in(text: &str) -> &str {
    let lower = text.to_ascii_lowercase();
    for lead in LEAD_INS {
        if lower.starts_with(lead) {
            let rest = text[lead.len()..].trim_start_matches([' ', ':', ',']).trim_start();
            return rest;
        }
    }
    text
}

/// "A woman ..." -> "a woman ..." (only articles: names and other words keep their case).
fn lower_article(text: &str) -> String {
    for article in ["A ", "An ", "The "] {
        if let Some(rest) = text.strip_prefix(article) {
            return format!("{}{rest}", article.to_ascii_lowercase());
        }
    }
    text.to_string()
}

/// A description cut at the token limit, without its unfinished last sentence (kept whole when
/// it has no finished sentence).
fn without_cut_sentence(text: &str) -> &str {
    match text.rfind(['.', '!', '?']) {
        Some(end) if end > text.len() / 3 => &text[..=end],
        _ => text,
    }
}

// ---------------------------------------------------------------------------------------------
// Result and cache
// ---------------------------------------------------------------------------------------------

/// A picture's description (from Bonsai or the cache).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Described {
    /// The picture file.
    pub path: PathBuf,
    pub role: RefRole,
    /// The cleaned paragraph.
    pub description: String,
    /// The model that wrote it (GGUF file name), when known.
    pub model: String,
    /// When it was written (local time, `2026-10-28 14:03:12`).
    pub created: String,
    /// It came from the cache (no request was sent).
    pub cached: bool,
    /// Time of the request (0 from the cache).
    pub elapsed_ms: u64,
    /// llama-server's numbers (`None` from the cache).
    pub timings: Option<Timings>,
    /// `stop`, or `length` when it hit [`MAX_TOKENS`] (then the cut sentence is dropped).
    pub finish_reason: String,
}

/// The finished request as a [`Described`]: the answer cleaned (an empty one is an error).
pub fn finish(path: &Path, role: RefRole, result: &ChatResult, model: &str) -> anyhow::Result<Described> {
    let mut description = clean_description(&result.content);
    if result.finish_reason == "length" {
        description = without_cut_sentence(&description).to_string();
    }
    if description.is_empty() {
        bail!(
            "Bonsai gave no description of {} (finish reason: {}) - try again or describe it in words",
            path.display(),
            if result.finish_reason.is_empty() { "unknown" } else { result.finish_reason.as_str() }
        );
    }
    Ok(Described {
        path: path.to_path_buf(),
        role,
        description,
        model: model.to_string(),
        created: now_text(),
        cached: false,
        elapsed_ms: result.elapsed_ms,
        timings: result.timings.clone(),
        finish_reason: result.finish_reason.clone(),
    })
}

fn now_text() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// One cache file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheEntry {
    pub description: String,
    pub model: String,
    pub created: String,
}

/// Descriptions by picture content and role: `<dir>\<sha256 of the file bytes>-<role id>.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionCache {
    pub dir: PathBuf,
}

impl DescriptionCache {
    /// The cache in `config_dir()\descriptions`.
    pub fn open() -> Self {
        Self::in_dir(crate::config::config_dir().join(CACHE_DIR))
    }

    /// A cache in `dir` (tests).
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The key of a picture's bytes and role: `<sha256 hex>-<role id>`.
    pub fn key(bytes: &[u8], role: RefRole) -> String {
        let digest = Sha256::digest(bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("{hex}-{}", role.id())
    }

    /// The key of a picture file (it is read).
    pub fn key_for_file(path: &Path, role: RefRole) -> anyhow::Result<String> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Self::key(&bytes, role))
    }

    fn file(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// The entry for `key` (`None` when missing, unreadable or empty).
    pub fn get(&self, key: &str) -> Option<CacheEntry> {
        let text = std::fs::read_to_string(self.file(key)).ok()?;
        let entry: CacheEntry = serde_json::from_str(&text).ok()?;
        (!entry.description.trim().is_empty()).then_some(entry)
    }

    /// Store `entry` under `key` (written through a temp file).
    pub fn put(&self, key: &str, entry: &CacheEntry) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let path = self.file(key);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(entry)?;
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }

    /// The cached description of a picture file and role, as a [`Described`].
    pub fn lookup(&self, path: &Path, role: RefRole) -> Option<Described> {
        let key = Self::key_for_file(path, role).ok()?;
        let entry = self.get(&key)?;
        Some(Described {
            path: path.to_path_buf(),
            role,
            description: entry.description,
            model: entry.model,
            created: entry.created,
            cached: true,
            elapsed_ms: 0,
            timings: None,
            finish_reason: "cache".to_string(),
        })
    }

    /// Remember a new description of a picture file (a failure only means it is asked again).
    pub fn store(&self, described: &Described) -> anyhow::Result<()> {
        let key = Self::key_for_file(&described.path, described.role)?;
        let entry = CacheEntry {
            description: described.description.clone(),
            model: described.model.clone(),
            created: described.created.clone(),
        };
        self.put(&key, &entry)
    }
}

/// Put a finished description into a reference: when its text is empty, still Bonsai's own, or
/// still `replaces` (the text it had when the user asked for the description). Text the user typed
/// since is never overwritten: returns false then (offer the description instead).
pub fn apply(r: &mut crate::request::RefImage, description: &str, replaces: &str) -> bool {
    let current = r.description.trim();
    if !(r.may_auto_describe() || current == replaces.trim()) {
        return false;
    }
    r.description = description.to_string();
    r.auto_description = Some(description.to_string());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::RefImage;

    #[test]
    fn requests_attach_the_picture_with_thinking_off() {
        let img = ImageAttachment { path: None, mime: "image/png".into(), base64: "AAAA".into() };
        for &role in RefRole::all() {
            let msgs = messages(img.clone(), role);
            let body = crate::gen::build_chat_body(&msgs, &params(), true);
            assert_eq!(body["messages"][0]["content"], SYSTEM_PROMPT, "{role}");
            let parts = body["messages"][1]["content"].as_array().expect("text + image parts");
            assert_eq!(parts.len(), 2);
            let text = parts[0]["text"].as_str().unwrap_or_default();
            assert!(text.starts_with(instruction(role)), "{role}: {text}");
            assert!(text.contains(&role.label().to_ascii_lowercase()), "{role}");
            assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
            assert_eq!(body["chat_template_kwargs"], serde_json::json!({ "enable_thinking": false }));
            assert_eq!(body["max_tokens"], MAX_TOKENS);
            assert!((body["temperature"].as_f64().unwrap_or_default() - 0.2).abs() < 1e-6);
        }
        assert!(instruction(RefRole::Place).contains("Leave out any people"));
        assert!(instruction(RefRole::Outfit).contains("only the outfit"));
        assert!(instruction(RefRole::Style).contains("not the content"));
        assert!(instruction(RefRole::FirstFrame).contains("camera angle"));
        assert_eq!(instruction(RefRole::LastFrame), instruction(RefRole::FirstFrame));
        assert!(instruction(RefRole::Person).contains("apparent age range"));
    }

    #[test]
    fn descriptions_are_cleaned_to_one_paragraph() {
        let cases = [
            ("This image shows a young woman with short black hair.", "a young woman with short black hair."),
            ("\"A red bicycle leaning on a brick wall.\"", "a red bicycle leaning on a brick wall."),
            ("**Description:** The image shows an empty cafe", "an empty cafe"),
            ("In this picture, a man in a grey suit stands", "a man in a grey suit stands"),
            ("- a woman\n- long hair\n\n1. red dress", "a woman long hair red dress"),
            ("```\nA cat on a sofa.\n```", "a cat on a sofa."),
            ("# Person\nThe woman wears a   blue\tcoat.", "the woman wears a blue coat."),
            ("\u{201c}Soft warm light, 35mm grain.\u{201d}", "Soft warm light, 35mm grain."),
            ("Here is a description of the picture: \"a narrow street\"", "a narrow street"),
            ("Paris rooftops at dusk", "Paris rooftops at dusk"),
            ("   ", ""),
        ];
        for (raw, want) in cases {
            assert_eq!(clean_description(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn a_cut_answer_loses_its_unfinished_sentence() {
        let result = ChatResult {
            content: "A woman in a red coat stands on a bridge. Behind her the river and the old".into(),
            finish_reason: "length".into(),
            elapsed_ms: 1234,
            ..ChatResult::default()
        };
        let d = finish(Path::new("x.png"), RefRole::Person, &result, "m.gguf").expect("described");
        assert_eq!(d.description, "a woman in a red coat stands on a bridge.");
        assert_eq!((d.elapsed_ms, d.cached, d.model.as_str()), (1234, false, "m.gguf"));
        let empty = ChatResult { content: "  \"\"  ".into(), finish_reason: "stop".into(), ..ChatResult::default() };
        assert!(finish(Path::new("x.png"), RefRole::Person, &empty, "").is_err());
    }

    #[test]
    fn roles_parse_from_their_ids_and_plain_words() {
        for &role in RefRole::all() {
            assert_eq!(role.id().parse::<RefRole>().expect("id"), role);
            assert_eq!(role.id().replace('-', "_").parse::<RefRole>().expect("snake case"), role);
        }
        assert_eq!("Background".parse::<RefRole>().expect("word"), RefRole::Place);
        assert_eq!(" first ".parse::<RefRole>().expect("word"), RefRole::FirstFrame);
        assert_eq!("look".parse::<RefRole>().expect("word"), RefRole::Style);
        let err = "hat".parse::<RefRole>().expect_err("unknown");
        assert!(err.to_string().contains("first-frame"), "{err}");
    }

    #[test]
    fn vision_is_required() {
        let err = check_vision(&ServerProps::default()).expect_err("no mmproj");
        assert!(format!("{err}").contains("no vision projector"));
        assert!(check_vision(&ServerProps { vision: true, ..ServerProps::default() }).is_ok());
    }

    #[test]
    fn cache_is_keyed_by_picture_bytes_and_role() {
        let dir = std::env::temp_dir().join(format!("bh3-describe-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = DescriptionCache::in_dir(dir.join(CACHE_DIR));
        let a = dir.join("a.png");
        let copy = dir.join("copy-of-a.png");
        let b = dir.join("b.png");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(&a, b"picture A").expect("a");
        std::fs::write(&copy, b"picture A").expect("copy");
        std::fs::write(&b, b"picture B").expect("b");
        let key = DescriptionCache::key(b"picture A", RefRole::Person);
        assert_eq!(key.len(), 64 + "-person".len());
        assert!(key.ends_with("-person"));
        assert_eq!(DescriptionCache::key_for_file(&a, RefRole::Person).expect("key"), key);
        assert_ne!(DescriptionCache::key(b"picture A", RefRole::Place), key);

        assert!(cache.lookup(&a, RefRole::Person).is_none(), "miss before");
        let described = Described {
            path: a.clone(),
            role: RefRole::Person,
            description: "a woman".into(),
            model: "bonsai.gguf".into(),
            created: "2026-10-28 12:00:00".into(),
            ..Described::default()
        };
        cache.store(&described).expect("store");
        let hit = cache.lookup(&copy, RefRole::Person).expect("same bytes, other file: hit");
        assert_eq!((hit.description.as_str(), hit.model.as_str(), hit.cached), ("a woman", "bonsai.gguf", true));
        assert_eq!(hit.path, copy);
        assert!(cache.lookup(&a, RefRole::Place).is_none(), "other role: miss");
        assert!(cache.lookup(&b, RefRole::Person).is_none(), "other picture: miss");
        assert!(cache.lookup(&dir.join("missing.png"), RefRole::Person).is_none());
        let file = cache.dir.join(format!("{key}.json"));
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).expect("file")).expect("json");
        assert_eq!(saved["description"], "a woman");
        assert_eq!(saved["created"], "2026-10-28 12:00:00");
        std::fs::write(&file, "not json").expect("break it");
        assert!(cache.lookup(&a, RefRole::Person).is_none(), "a broken file is a miss");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_text_is_never_overwritten() {
        // Empty: written, marked as Bonsai's.
        let mut r = RefImage::default();
        assert!(r.may_auto_describe());
        assert!(apply(&mut r, "a woman", ""));
        assert_eq!(r.description, "a woman");
        assert!(r.description_is_auto());
        // Still Bonsai's untouched text (e.g. the role changed): replaced.
        assert!(apply(&mut r, "a cafe", ""));
        assert_eq!(r.description, "a cafe");
        // The user edits it: it is the user's now.
        r.description.push_str(" with tall windows");
        assert!(!r.description_is_auto());
        assert!(!r.may_auto_describe());
        assert!(!apply(&mut r, "a bar", ""), "auto describe never overwrites typed text");
        assert_eq!(r.description, "a cafe with tall windows");
        // Asked for explicitly ("Describe with Bonsai"): replaces the text it was asked for...
        assert!(apply(&mut r, "a bar", "a cafe with tall windows"));
        assert_eq!(r.description, "a bar");
        // ...but not what the user typed while Bonsai was looking.
        let mut typed = RefImage { description: "my own words".into(), ..RefImage::default() };
        let asked_with = typed.description.clone();
        typed.description = "my own words, changed".into();
        assert!(!apply(&mut typed, "a bar", &asked_with));
        assert_eq!(typed.description, "my own words, changed");
        assert_eq!(typed.auto_description, None);
    }
}
