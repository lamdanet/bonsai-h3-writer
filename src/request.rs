//! Structured request builder: turns form fields into the plain-language request sent to Bonsai
//! (same shape as `assets/library/templates.txt`). Bonsai then writes the H3 prompt.
//!
//! Example (r2v):
//! ```text
//! Ref 1: a young woman, long black hair in a high bun, cream crochet one-piece swimsuit (person).
//! Ref 2: an empty sunlit cafe with tall windows (place). Use the place from Ref 2.
//! Outfit: a white linen sundress.
//! A young man with short dark hair, blue shirt (no photo) is also in the scene.
//! 6 s (real length 6.58 s), one take: she walks to the window table and sits down. Camera: the camera pushes in slowly. Style: film look: ...
//! She says "Good morning."
//! ```
//! Ref numbers are the list order (= the Load Image order in ComfyUI); roles never reorder them.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::gen::{ImageAttachment, Mode};
use crate::lengths;
use crate::lint::LintContext;

/// Most reference images an r2v prompt can use (`<Picture 1>`..`<Picture 9>`).
pub const MAX_R2V_REFS: usize = 9;
/// f2v uses a first frame and optionally a last frame.
pub const MAX_F2V_REFS: usize = 2;

/// Approximate prompt tokens one picture costs on Bonsai's vision input at most: pictures are
/// sent with their longest side at 1024 px or less ([`ImageAttachment::from_file`] scales big ones
/// down, also on CUDA where llama-server does not cap them), about one token per 32x32 px.
pub const IMAGE_TOKENS: usize = 1_100;
/// Approximate seconds one such picture adds on the RTX 4060 laptop.
pub const IMAGE_SECONDS: usize = 18;

/// Start of the request line that lists the pictures sent along (see [`RequestSpec::attached_line`]).
const ATTACHED_PREFIX: &str = "Attached pictures, in this order:";

/// Most pictures a mode can use ([`MAX_R2V_REFS`] / [`MAX_F2V_REFS`]).
pub fn max_refs(mode: Mode) -> usize {
    match mode {
        Mode::R2v => MAX_R2V_REFS,
        Mode::F2v => MAX_F2V_REFS,
    }
}

/// What a reference picture provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefRole {
    /// A person's look (face, hair, figure, outfit).
    #[default]
    Person,
    /// Another photo of the same person (another angle / outfit).
    SamePerson,
    /// A place / environment.
    Place,
    /// A person and the place they are in.
    PersonAndPlace,
    /// An outfit or clothing item to put on someone.
    Outfit,
    /// An object / prop.
    Prop,
    /// Only the look: colours, light, grain, style.
    Style,
    /// The exact first frame (r2v: First-frame pin ON; f2v: Picture 1).
    FirstFrame,
    /// The exact last frame (f2v: Picture 2).
    LastFrame,
    /// A keyframe of a later shot (r2v).
    Keyframe,
    /// A storyboard / shot plan (r2v).
    Storyboard,
}

impl RefRole {
    /// Every role, in UI order.
    pub fn all() -> &'static [RefRole] {
        &[
            RefRole::Person,
            RefRole::SamePerson,
            RefRole::Place,
            RefRole::PersonAndPlace,
            RefRole::Outfit,
            RefRole::Prop,
            RefRole::Style,
            RefRole::FirstFrame,
            RefRole::LastFrame,
            RefRole::Keyframe,
            RefRole::Storyboard,
        ]
    }
    /// UI label, e.g. "Person", "Same person (other photo)".
    pub fn label(&self) -> &'static str {
        match self {
            RefRole::Person => "Person",
            RefRole::SamePerson => "Same person (other photo)",
            RefRole::Place => "Place / background",
            RefRole::PersonAndPlace => "Person + place",
            RefRole::Outfit => "Outfit",
            RefRole::Prop => "Object / prop",
            RefRole::Style => "Style / look only",
            RefRole::FirstFrame => "Exact first frame",
            RefRole::LastFrame => "Exact last frame",
            RefRole::Keyframe => "Keyframe of a shot",
            RefRole::Storyboard => "Storyboard",
        }
    }
    /// Roles that make sense in a mode (f2v: FirstFrame, LastFrame only).
    pub fn allowed_in(&self, mode: Mode) -> bool {
        match mode {
            Mode::F2v => matches!(self, RefRole::FirstFrame | RefRole::LastFrame),
            Mode::R2v => !matches!(self, RefRole::LastFrame),
        }
    }
}

/// One reference picture.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RefImage {
    pub role: RefRole,
    /// What the picture shows, in words (sent to Bonsai).
    pub description: String,
    /// Optional file (thumbnail in the UI; sent to Bonsai only when `send_image`).
    pub image_path: Option<PathBuf>,
    /// Send the pixels to Bonsai's vision input (~1,100 tokens and ~18 s each).
    pub send_image: bool,
}

/// Look of the clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StylePreset {
    /// Film look: 35mm, soft grain, warm grade, shallow depth of field.
    #[default]
    Film35mm,
    Natural,
    Documentary,
    Commercial,
    Noir,
    Anime,
    /// Use `style_custom`.
    Custom,
}

impl StylePreset {
    /// Every preset, in UI order.
    pub fn all() -> &'static [StylePreset] {
        &[
            StylePreset::Film35mm,
            StylePreset::Natural,
            StylePreset::Documentary,
            StylePreset::Commercial,
            StylePreset::Noir,
            StylePreset::Anime,
            StylePreset::Custom,
        ]
    }
    /// UI label.
    pub fn label(&self) -> &'static str {
        match self {
            StylePreset::Film35mm => "Film (35mm)",
            StylePreset::Natural => "Natural",
            StylePreset::Documentary => "Documentary",
            StylePreset::Commercial => "Commercial",
            StylePreset::Noir => "Noir",
            StylePreset::Anime => "Anime",
            StylePreset::Custom => "Custom",
        }
    }
    /// Words used in the request ("film look: 35mm, soft grain, warm grade, shallow depth of field").
    pub fn words(&self) -> &'static str {
        match self {
            StylePreset::Film35mm => "film look: 35mm anamorphic, soft grain, warm grade, shallow depth of field",
            StylePreset::Natural => "natural live-action look, soft daylight, true colours",
            StylePreset::Documentary => "handheld documentary look, available light, natural colours",
            StylePreset::Commercial => "glossy commercial look, bright clean light, crisp detail",
            StylePreset::Noir => "black-and-white film noir look, hard light, deep shadows",
            StylePreset::Anime => "2D anime look, clean line art, cel shading",
            StylePreset::Custom => "",
        }
    }
}

/// Camera preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraPreset {
    /// Let Bonsai choose.
    #[default]
    Auto,
    Static,
    PushIn,
    PullOut,
    Tracking,
    Arc,
    Handheld,
    /// Very close side view from the bust up.
    CloseSideView,
    /// Use `camera_custom`.
    Custom,
}

impl CameraPreset {
    /// Every preset, in UI order.
    pub fn all() -> &'static [CameraPreset] {
        &[
            CameraPreset::Auto,
            CameraPreset::Static,
            CameraPreset::PushIn,
            CameraPreset::PullOut,
            CameraPreset::Tracking,
            CameraPreset::Arc,
            CameraPreset::Handheld,
            CameraPreset::CloseSideView,
            CameraPreset::Custom,
        ]
    }
    /// UI label.
    pub fn label(&self) -> &'static str {
        match self {
            CameraPreset::Auto => "Auto",
            CameraPreset::Static => "Static",
            CameraPreset::PushIn => "Push in",
            CameraPreset::PullOut => "Pull out",
            CameraPreset::Tracking => "Tracking",
            CameraPreset::Arc => "Arc around",
            CameraPreset::Handheld => "Handheld",
            CameraPreset::CloseSideView => "Close side view (bust up)",
            CameraPreset::Custom => "Custom",
        }
    }
    /// Words used in the request, empty for Auto/Custom.
    pub fn words(&self) -> &'static str {
        match self {
            CameraPreset::Auto | CameraPreset::Custom => "",
            CameraPreset::Static => "static shot",
            CameraPreset::PushIn => "the camera pushes in slowly",
            CameraPreset::PullOut => "the camera pulls out slowly",
            CameraPreset::Tracking => "tracking shot that follows them",
            CameraPreset::Arc => "the camera arcs slowly around them",
            CameraPreset::Handheld => "handheld camera with a slight natural drift",
            CameraPreset::CloseSideView => "very close side view from the bust up, shallow depth of field",
        }
    }
}

/// A line of dialogue (exact words are kept).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DialogueLine {
    /// Who speaks, e.g. "she", "the man", "Ref 1".
    pub speaker: String,
    pub words: String,
}

/// Everything the request form holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestSpec {
    pub mode: Mode,
    pub refs: Vec<RefImage>,
    /// Requested seconds (see [`crate::lengths`]).
    pub duration_s: u32,
    /// 0 = one continuous take.
    pub cuts: u32,
    /// What happens (one continuous action per shot works best).
    pub action: String,
    /// Where it happens / light / time of day (when no picture gives the place).
    pub place: String,
    /// Outfit to use (empty = keep the pictures' outfit).
    pub outfit: String,
    pub style: StylePreset,
    pub style_custom: String,
    pub camera: CameraPreset,
    pub camera_custom: String,
    pub dialogue: Vec<DialogueLine>,
    /// People without a picture ("a young man with short dark hair, blue shirt").
    pub extra_people: String,
    /// Anything else for Bonsai.
    pub notes: String,
    /// r2v: Ref 1 is pinned as the first frame in ComfyUI (First-frame pin ON).
    pub first_frame_pinned: bool,
}

impl Default for RequestSpec {
    fn default() -> Self {
        Self {
            mode: Mode::R2v,
            refs: vec![RefImage::default()],
            duration_s: 6,
            cuts: 0,
            action: String::new(),
            place: String::new(),
            outfit: String::new(),
            style: StylePreset::Film35mm,
            style_custom: String::new(),
            camera: CameraPreset::Auto,
            camera_custom: String::new(),
            dialogue: Vec::new(),
            extra_people: String::new(),
            notes: String::new(),
            first_frame_pinned: false,
        }
    }
}

impl RequestSpec {
    /// The request text for Bonsai, e.g. (r2v):
    /// ```text
    /// Ref 1: <description> (person).
    /// Ref 2: <description> (place). Use the place from Ref 2.
    /// Outfit: <outfit>.
    /// A young man ... (no photo) ...
    /// 6 s (real length 6.58 s), one take: <action>. Camera: <...>. Style: <...>.
    /// She says "<words>".
    /// <notes>
    /// ```
    /// f2v uses `Picture 1:` / `Picture 2:` wording. With cuts: "6 s (real length 6.58 s), 2 cuts: ...".
    /// First frame pinned (r2v): "Ref 1 is the first frame: ...".
    pub fn build_text(&self) -> String {
        let refs = self.used_refs();
        let mut lines: Vec<String> = refs.iter().enumerate().map(|(i, r)| self.ref_line(i, r)).collect();
        let attached: Vec<String> = refs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.send_image && r.image_path.is_some())
            .map(|(i, _)| self.ref_name(i))
            .collect();
        lines.extend(Self::attached_line(&attached));
        let outfit = clean(&self.outfit);
        if !outfit.is_empty() {
            lines.push(outfit_sentence(outfit));
        }
        let extra = clean(&self.extra_people);
        if !extra.is_empty() {
            lines.push(extra_people_sentence(extra));
        }
        let place = clean(&self.place);
        if !place.is_empty() {
            lines.push(format!("Place: {place}."));
        }
        lines.push(self.shot_line());
        lines.extend(self.dialogue_lines());
        let notes = self.notes.trim();
        if !notes.is_empty() {
            lines.push(notes.to_string());
        }
        lines.join("\n")
    }

    /// The `Attached pictures, in this order: Ref 1, Ref 2.` line for the pictures `names`
    /// (`None` without pictures).
    pub fn attached_line(names: &[String]) -> Option<String> {
        (!names.is_empty()).then(|| format!("{ATTACHED_PREFIX} {}.", names.join(", ")))
    }

    /// Problems worth showing before sending (too many refs for the mode, f2v without a first frame,
    /// empty action, more cuts than the length allows, FirstFrame role not on Ref 1, ...).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let max = max_refs(self.mode);
        if self.refs.len() > max {
            out.push(match self.mode {
                Mode::R2v => format!(
                    "r2v uses at most {MAX_R2V_REFS} reference pictures (Ref 1 to Ref 9): the {} after Ref 9 are left out",
                    self.refs.len() - max
                ),
                Mode::F2v => format!(
                    "f2v uses only a first frame and an optional last frame: the {} picture(s) after Picture 2 are left out",
                    self.refs.len() - max
                ),
            });
        }
        match self.mode {
            Mode::F2v => self.f2v_role_warnings(&mut out),
            Mode::R2v => self.r2v_role_warnings(&mut out),
        }
        for (i, r) in self.used_refs().iter().enumerate() {
            let name = self.ref_name(i);
            if r.description.trim().is_empty() && !r.send_image {
                out.push(format!(
                    "{name} has no description: describe the picture in words (or turn on \"send to Bonsai\")"
                ));
            }
            if r.send_image && r.image_path.is_none() {
                out.push(format!("{name}: \"send to Bonsai\" is on but no image file is chosen"));
            }
        }
        if self.action.trim().is_empty() {
            out.push(
                "The action is empty: describe what happens (one continuous action per shot works best)".to_string(),
            );
        }
        if self.duration_s == 0 {
            out.push("The duration is 0 s: pick a length (3 s for tests, 6-15 s for finals)".to_string());
        }
        let max_cuts = lengths::suggested_max_cuts(self.duration_s as f32);
        if self.cuts > max_cuts {
            out.push(format!(
                "{} cuts in {} s is a lot: about one cut per 3 s works best (up to {max_cuts} here)",
                self.cuts, self.duration_s
            ));
        }
        let sent = self.sent_image_count();
        if sent > 0 {
            out.push(format!(
                "{sent} picture(s) sent to Bonsai: each costs ~{} tokens and ~{IMAGE_SECONDS} s (~{} tokens in total); \
                 describing them in words is cheaper",
                thousands(IMAGE_TOKENS),
                thousands(sent * IMAGE_TOKENS)
            ));
        }
        out
    }

    /// Lint context matching this request.
    pub fn lint_context(&self) -> LintContext {
        LintContext {
            mode: self.mode,
            duration_s: (self.duration_s > 0).then_some(self.duration_s as f32),
            ref_count: Some(self.used_refs().len()),
            first_frame_pinned: (self.mode == Mode::R2v).then_some(self.first_frame_pinned),
            has_last_frame: (self.mode == Mode::F2v).then(|| self.has_last_frame()),
        }
    }

    /// Images to send to Bonsai's vision input (refs with `send_image` and a path).
    pub fn images(&self) -> anyhow::Result<Vec<ImageAttachment>> {
        let mut out = Vec::new();
        for (i, r) in self.used_refs().iter().enumerate() {
            let (true, Some(path)) = (r.send_image, r.image_path.as_ref()) else { continue };
            let image = ImageAttachment::from_file(path)
                .with_context(|| format!("loading the picture of {} ({})", self.ref_name(i), path.display()))?;
            out.push(image);
        }
        Ok(out)
    }

    /// [`Self::images`] for a follow-up, one picture at a time: the pictures that can still be
    /// read with their names (`Ref 1`), and the names of those that cannot (moved or deleted).
    pub fn readable_images(&self) -> (Vec<(String, ImageAttachment)>, Vec<String>) {
        let (mut read, mut missing) = (Vec::new(), Vec::new());
        for (i, r) in self.used_refs().iter().enumerate() {
            let (true, Some(path)) = (r.send_image, r.image_path.as_ref()) else { continue };
            match ImageAttachment::from_file(path) {
                Ok(image) => read.push((self.ref_name(i), image)),
                Err(_) => missing.push(self.ref_name(i)),
            }
        }
        (read, missing)
    }

    /// f2v with a LastFrame ref (FL2VA).
    pub fn has_last_frame(&self) -> bool {
        self.mode == Mode::F2v && self.refs.iter().any(|r| r.role == RefRole::LastFrame)
    }

    /// Short title for history lists (first words of the action, else of Ref 1).
    pub fn title(&self) -> String {
        const TITLE_WORDS: usize = 6;
        let ref1 = self.refs.first().map_or("", |r| r.description.as_str());
        let Some(source) = [self.action.as_str(), ref1].into_iter().find(|s| !s.trim().is_empty()) else {
            return "Untitled".to_string();
        };
        let words: Vec<&str> = source.split_whitespace().take(TITLE_WORDS).collect();
        let joined = words.join(" ");
        let title = capitalize(joined.trim_end_matches([',', '.', ';', ':', '!', '?', '-']));
        if title.is_empty() {
            "Untitled".to_string()
        } else {
            title
        }
    }

    /// Pictures sent to Bonsai's vision input (`send_image` with a file).
    pub fn sent_image_count(&self) -> usize {
        self.used_refs().iter().filter(|r| r.send_image && r.image_path.is_some()).count()
    }

    /// The refs the mode can use (the first 9 in r2v, the first 2 in f2v).
    fn used_refs(&self) -> &[RefImage] {
        &self.refs[..self.refs.len().min(max_refs(self.mode))]
    }

    /// `Ref N` (r2v) / `Picture N` (f2v) for the ref at `index`.
    fn ref_name(&self, index: usize) -> String {
        match self.mode {
            Mode::R2v => format!("Ref {}", index + 1),
            Mode::F2v => format!("Picture {}", index + 1),
        }
    }

    fn ref_line(&self, index: usize, r: &RefImage) -> String {
        let n = index + 1;
        let desc = match clean(&r.description) {
            "" if r.send_image => "(see the picture)",
            d => d,
        };
        match self.mode {
            Mode::F2v => {
                // Picture 1 is always the first frame; Picture 2 is the last frame when marked so.
                let role = match r.role {
                    _ if index == 0 => "first frame".to_string(),
                    RefRole::LastFrame => "last frame".to_string(),
                    RefRole::FirstFrame => "first frame".to_string(),
                    other => other.label().to_ascii_lowercase(),
                };
                join_ref(&format!("Picture {n}"), desc, &role)
            }
            Mode::R2v if index == 0 && self.first_frame_pinned => {
                if desc.is_empty() {
                    "Ref 1 is the first frame.".to_string()
                } else {
                    format!("Ref 1 is the first frame: {desc}.")
                }
            }
            Mode::R2v => {
                let (role, extra) = match r.role {
                    RefRole::Person => ("person".to_string(), None),
                    RefRole::SamePerson => (self.same_person_words(index), None),
                    RefRole::Place => ("place".to_string(), Some(format!("Use the place from Ref {n}."))),
                    RefRole::PersonAndPlace => {
                        ("person and place".to_string(), Some(format!("Use the place from Ref {n}.")))
                    }
                    RefRole::Outfit => ("outfit".to_string(), Some(format!("Use the outfit from Ref {n}."))),
                    RefRole::Prop => ("object".to_string(), None),
                    RefRole::Style => ("style only".to_string(), Some(format!("Only copy the look of Ref {n}."))),
                    RefRole::FirstFrame => ("first frame".to_string(), None),
                    RefRole::LastFrame => ("last frame".to_string(), None),
                    RefRole::Keyframe => ("keyframe of a later shot".to_string(), None),
                    RefRole::Storyboard => {
                        ("storyboard".to_string(), Some("Follow its shot order and framing.".to_string()))
                    }
                };
                let mut line = join_ref(&format!("Ref {n}"), desc, &role);
                if let Some(extra) = extra {
                    line.push(' ');
                    line.push_str(&extra);
                }
                line
            }
        }
    }

    /// "same person as Ref K, another photo" (K = the nearest earlier person picture).
    fn same_person_words(&self, index: usize) -> String {
        let earlier = self.refs[..index].iter().enumerate().rev().find(|(i, r)| {
            matches!(r.role, RefRole::Person | RefRole::PersonAndPlace | RefRole::FirstFrame)
                || (*i == 0 && self.first_frame_pinned)
        });
        match earlier {
            Some((i, _)) => format!("same person as Ref {}, another photo", i + 1),
            None => "the same person, another photo".to_string(),
        }
    }

    /// "6 s (real length 6.58 s), one take: <action>. Camera: <...>. Style: <...>." The real length
    /// is spelled out, so Bonsai never has to work it out (the FL2VA line and the shot times need it).
    fn shot_line(&self) -> String {
        let take = match (self.cuts, self.mode) {
            (0, Mode::R2v) => "one take".to_string(),
            (0, Mode::F2v) => "one shot".to_string(),
            (1, _) => "1 cut".to_string(),
            (n, _) => format!("{n} cuts"),
        };
        let length = match self.duration_s {
            0 => "0 s".to_string(),
            d => format!("{d} s (real length {} s)", lengths::format_real(d as f32)),
        };
        let action = clean(&self.action);
        let mut line = if action.is_empty() {
            format!("{length}, {take}.")
        } else {
            format!("{length}, {take}: {}", sentence(action))
        };
        let camera = match self.camera {
            CameraPreset::Custom => clean(&self.camera_custom),
            preset => preset.words(),
        };
        if !camera.is_empty() {
            line.push_str(&format!(" Camera: {}", sentence(camera)));
        }
        let style = match self.style {
            StylePreset::Custom => clean(&self.style_custom),
            preset => preset.words(),
        };
        if !style.is_empty() {
            line.push_str(&format!(" Style: {}", sentence(style)));
        }
        line
    }

    /// `She says "..."`, then `He answers "..."` (exact words kept, Bonsai adds the ids and `<d>` tags).
    fn dialogue_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut previous: Option<String> = None;
        for d in &self.dialogue {
            let words = d.words.trim().replace('"', "'");
            if words.is_empty() {
                continue;
            }
            let words = if words.ends_with(['.', '!', '?', '…']) { words } else { format!("{words}.") };
            let speaker = match d.speaker.trim() {
                "" => "Someone".to_string(),
                s => capitalize(s),
            };
            let verb = match &previous {
                None => "says",
                Some(p) if p.eq_ignore_ascii_case(&speaker) => "adds",
                Some(_) => "answers",
            };
            out.push(format!("{speaker} {verb} \"{words}\""));
            previous = Some(speaker);
        }
        out
    }

    fn f2v_role_warnings(&self, out: &mut Vec<String>) {
        match self.refs.first() {
            None => out.push("f2v needs Picture 1: add the exact first frame".to_string()),
            Some(r) if r.role != RefRole::FirstFrame => out.push(
                "f2v: Picture 1 must be the exact first frame (set its role to \"Exact first frame\")".to_string(),
            ),
            Some(_) => {}
        }
        for (i, r) in self.refs.iter().enumerate() {
            if r.role == RefRole::LastFrame && i != 1 {
                out.push(format!("Picture {}: the exact last frame must be Picture 2", i + 1));
            }
        }
        if let Some(r) = self.refs.get(1) {
            if r.role != RefRole::LastFrame {
                out.push("f2v: Picture 2 is the last frame (set its role to \"Exact last frame\")".to_string());
            }
        }
    }

    fn r2v_role_warnings(&self, out: &mut Vec<String>) {
        if self.refs.is_empty() {
            out.push("r2v needs at least one reference picture".to_string());
        }
        for (i, r) in self.refs.iter().enumerate() {
            if r.role == RefRole::FirstFrame && i != 0 {
                out.push(format!(
                    "Ref {}: only Ref 1 can be the exact first frame (First-frame pin): move it to the top",
                    i + 1
                ));
            }
            if !r.role.allowed_in(Mode::R2v) {
                out.push(format!(
                    "Ref {}: \"{}\" is an f2v role; in r2v use \"Keyframe of a shot\"",
                    i + 1,
                    r.role.label()
                ));
            }
        }
        let ref1_is_frame = self.refs.first().is_some_and(|r| r.role == RefRole::FirstFrame);
        if self.first_frame_pinned && !ref1_is_frame {
            out.push("The First-frame pin is on, but Ref 1 is not marked as the exact first frame".to_string());
        }
        if ref1_is_frame && !self.first_frame_pinned {
            out.push(
                "Ref 1 is marked as the exact first frame: turn the First-frame pin on (here and in ComfyUI)"
                    .to_string(),
            );
        }
    }
}

/// `Ref 1: <desc> (<role>).` / `Ref 1: (<role>).` when there is no description.
fn join_ref(name: &str, desc: &str, role: &str) -> String {
    if desc.is_empty() {
        format!("{name}: ({role}).")
    } else {
        format!("{name}: {desc} ({role}).")
    }
}

/// "Outfit: <outfit>." (no he/she guess) unless the text already is a sentence about wearing it.
fn outfit_sentence(outfit: &str) -> String {
    let lower = outfit.to_ascii_lowercase();
    if lower.contains("wear") || lower.starts_with("use ") || lower.starts_with("keep ") {
        sentence(&capitalize(outfit))
    } else {
        format!("Outfit: {}", sentence(outfit))
    }
}

/// "<Person> (no photo) is also in the scene." (kept as written when it already says "no photo").
fn extra_people_sentence(extra: &str) -> String {
    let lower = extra.to_ascii_lowercase();
    if lower.contains("no photo") {
        return sentence(&capitalize(extra));
    }
    let plural = ["two ", "three ", "four ", "several ", "some ", "a few ", "a group", "a couple", "people", "friends"]
        .iter()
        .any(|p| lower.starts_with(p))
        || [" and a ", " and an ", " and the ", " and two ", " and some "].iter().any(|p| lower.contains(p));
    let verb = if plural { "are" } else { "is" };
    format!("{} (no photo) {verb} also in the scene.", capitalize(extra))
}

/// Trim and drop trailing periods (the builder adds its own punctuation).
fn clean(text: &str) -> &str {
    text.trim().trim_end_matches(|c: char| c == '.' || c.is_whitespace())
}

/// `text` ending with `.`, `!` or `?`.
fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.ends_with(['.', '!', '?']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// First letter upper-case.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `1100` -> `1,100`.
fn thousands(n: usize) -> String {
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

// ---------------------------------------------------------------------------------------------
// Check facts from a typed request
// ---------------------------------------------------------------------------------------------

/// `ctx` with its unknown facts taken from `from`.
pub fn fill_missing(mut ctx: LintContext, from: &LintContext) -> LintContext {
    ctx.duration_s = ctx.duration_s.or(from.duration_s);
    ctx.ref_count = ctx.ref_count.or(from.ref_count);
    ctx.first_frame_pinned = ctx.first_frame_pinned.or(from.first_frame_pinned);
    ctx.has_last_frame = ctx.has_last_frame.or(from.has_last_frame);
    ctx
}

fn cached_regex(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

/// `text` with its `Attached pictures, in this order: ...` line listing only `names` (the line goes
/// when `names` is empty), for a follow-up that cannot send every picture of the first turn again.
pub fn with_attached_line(text: &str, names: &[String]) -> String {
    let attached = RequestSpec::attached_line(names);
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        match (line.starts_with(ATTACHED_PREFIX), attached.as_deref()) {
            (false, _) => lines.push(line),
            (true, Some(new)) => lines.push(new),
            (true, None) => {}
        }
    }
    lines.join("\n")
}

/// Lengths a request can name: 1-30 s (H3 clips are 3-20 s); a "stray" bigger number is never one.
pub(crate) const PLAUSIBLE_SECONDS: std::ops::RangeInclusive<f32> = 1.0..=30.0;

/// `6 s`, `<6> s`, `10-second`, `15s`, `8 seconds` (1 = number, 2 = gap, 3 = unit).
fn duration_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, r"(?i)(?:^|[^\w.])<?(\d{1,3}(?:\.\d{1,3})?)>?(\s*-?\s*)(s|secs?|seconds?)\b")
}

/// What follows a length that names the clip: `, one take`, ` with 2 cuts`, `-second shot`,
/// ` clip`, ` long`, also behind a `(real length 6.58 s)` note. Group `shot` = a bare `shot`.
fn length_after_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(
        &RE,
        r"(?i)^\s*(?:\([^)\n]*\)\s*)?[,:;]?\s*<?\s*(?:(?:with\s+)?(?:\d+|one|two|three|four|five|six|a|no)\s+cuts?\b|(?:(?:one|single|a|1|continuous|unbroken)\s+)+(?:take|shot)\b|take\b|(?P<shot>shot)\b|(?:clip|video|long)\b)",
    )
}

/// `Length: 8 s`, `duration of 10 s` (the text before the number).
fn length_before_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, r"(?i)\b(?:length|duration)\s*(?:of\s+|is\s+|=\s*|:\s*)?(?:about\s+|around\s+)?<?\s*$")
}

/// `real length 6.58 s`, `really 6.58 s long`: the rendered length, not the requested one.
fn real_length_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, r"(?i)\breal(?:ly)?(?:\s+length)?\s*(?:of\s+|is\s+|=\s*|:\s*)?(?:about\s+)?<?\s*$")
}

/// `Ref 3`, `Picture 2`, `<Picture 1>` (1 = ref / picture, 2 = the number).
fn picture_number_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, r"(?i)\b(ref|picture)\s*([1-9])\b")
}

/// `Ref 1 is the first frame`, `Ref 1 (exact first frame)` (group 1 = `first frame`).
fn pinned_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, r"(?i)\bref\s*1\b[^.\n]{0,40}?\b(first[\s-]+frame)\b")
}

/// `Picture 2`, `last frame`.
fn last_frame_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    cached_regex(&RE, r"(?i)\bpicture\s*2\b|\blast[\s-]+frame\b")
}

/// The last `max` bytes of `text` (at a char boundary).
fn tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The clause before byte `at` negates what follows: `not the first frame`, `no last frame`,
/// `without a last frame`, `isn't Picture 2`. Clauses end at `. , ; : ( )` and line breaks.
fn negated(text: &str, at: usize) -> bool {
    let before = &text[..at];
    let start = before.rfind(['.', ',', ';', ':', '(', ')', '!', '?', '\n']).map_or(0, |i| i + 1);
    before[start..].split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '\u{2019}')).any(|word| {
        let word = word.to_lowercase().replace('\u{2019}', "'");
        matches!(word.as_str(), "no" | "not" | "without" | "never" | "nor") || word.ends_with("n't")
    })
}

/// Length mentions of a request: (seconds, next to the take / cut phrase). Left out: numbers
/// outside [`PLAUSIBLE_SECONDS`], real-length notes, and ages and decades (`in her 20s`,
/// `90s look`; `1990s` never matches) - `20s` / `30s` written without a space count only right
/// before `take`, `cuts`, `clip`, `video` or `long`.
fn length_mentions(text: &str) -> Vec<(f32, bool)> {
    let (Some(re), Some(after_re), Some(before_re), Some(real_re)) =
        (duration_re(), length_after_re(), length_before_re(), real_length_re())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for caps in re.captures_iter(text) {
        let (Some(all), Some(number), Some(gap), Some(unit)) = (caps.get(0), caps.get(1), caps.get(2), caps.get(3))
        else {
            continue;
        };
        let Ok(value) = number.as_str().parse::<f32>() else { continue };
        let before = tail(&text[..number.start()], 40);
        if !PLAUSIBLE_SECONDS.contains(&value) || real_re.is_match(before) {
            continue;
        }
        let after = after_re.captures(&text[all.end()..]);
        let decade = gap.as_str().is_empty()
            && unit.as_str().eq_ignore_ascii_case("s")
            && value.fract() == 0.0
            && value >= 20.0
            && (value as u32).is_multiple_of(10);
        // A decade form needs a take / cut / clip word right after it (a bare "shot" is not enough).
        if decade && after.as_ref().is_none_or(|c| c.name("shot").is_some()) {
            continue;
        }
        out.push((value, after.is_some() || before_re.is_match(before)));
    }
    out
}

/// The clip length a request names: the length next to the take / cut phrase, else the only
/// length it mentions; unknown when those disagree (`6 s: she waits 2 seconds`).
fn request_length(text: &str) -> Option<f32> {
    let mentions = length_mentions(text);
    let anchored: Vec<f32> = mentions.iter().filter(|(_, next_to_take)| *next_to_take).map(|(s, _)| *s).collect();
    let pool = if anchored.is_empty() { mentions.iter().map(|(s, _)| *s).collect() } else { anchored };
    let first = *pool.first()?;
    pool.iter().all(|s| (s - first).abs() < 1e-3).then_some(first)
}

/// Check facts read from a text request, the way the request templates write them: the length
/// ([`request_length`]: `6 s, one take: ...`), the highest `Ref N` / `Picture N` (f2v: `Picture N`
/// only, so an r2v request read after a mode switch gives no last frame), `Ref 1 is the first
/// frame` (r2v) and `Picture 2` / `last frame` (f2v); negated mentions (`no last frame`) do not
/// count. A missing length or picture count stays unknown; the pin (r2v) and the last frame (f2v)
/// count as off when not mentioned, because the request is the only place Bonsai learns about them.
pub fn context_from_text(mode: Mode, request: &str) -> LintContext {
    let duration_s = request_length(request);
    let highest = picture_number_re().and_then(|re| {
        re.captures_iter(request)
            .filter_map(|caps| {
                let (kind, number) = (caps.get(1)?, caps.get(2)?);
                let counts = mode == Mode::R2v || kind.as_str().eq_ignore_ascii_case("picture");
                (counts && !negated(request, kind.start())).then(|| number.as_str().parse::<usize>().ok())?
            })
            .max()
    });
    let affirmed = |re: Option<&Regex>, group: usize| {
        re.is_some_and(|re| {
            re.captures_iter(request).any(|caps| caps.get(group).is_some_and(|m| !negated(request, m.start())))
        })
    };
    match mode {
        Mode::R2v => LintContext {
            mode,
            duration_s,
            ref_count: highest,
            first_frame_pinned: Some(affirmed(pinned_re(), 1)),
            has_last_frame: None,
        },
        Mode::F2v => {
            let last = affirmed(last_frame_re(), 0) || highest.is_some_and(|n| n >= 2);
            LintContext {
                mode,
                duration_s,
                ref_count: Some(if last { 2 } else { 1 }),
                first_frame_pinned: None,
                has_last_frame: Some(last),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lint;

    #[test]
    fn request_facts_are_read_like_the_templates_write_them() {
        let r2v = "Ref 1: a young woman with short black hair. Ref 2: a rainy neon street at night (place).\n\
                   6 s, one take: she turns to the camera and smiles. Style: 35mm film look.";
        let c = context_from_text(Mode::R2v, r2v);
        assert_eq!(c.duration_s, Some(6.0));
        assert_eq!(c.ref_count, Some(2));
        assert_eq!(c.first_frame_pinned, Some(false));
        assert_eq!(c.has_last_frame, None);

        let pinned = "Ref 1 is the first frame: a woman at a cafe table. <10> s, 2 cuts: she stands up.";
        let c = context_from_text(Mode::R2v, pinned);
        assert_eq!((c.duration_s, c.ref_count, c.first_frame_pinned), (Some(10.0), Some(1), Some(true)));

        let f2v = "Picture 1: a man at an oak table. Picture 2: the man at the door. A 4-second shot: he walks out.";
        let c = context_from_text(Mode::F2v, f2v);
        assert_eq!((c.duration_s, c.ref_count, c.has_last_frame), (Some(4.0), Some(2), Some(true)));
        let c = context_from_text(Mode::F2v, "Picture 1: a woman. 3s: she waves.");
        assert_eq!((c.duration_s, c.ref_count, c.has_last_frame), (Some(3.0), Some(1), Some(false)));

        let none = context_from_text(Mode::R2v, "A 25-year-old woman with 3 sisters walks in.");
        assert_eq!((none.duration_s, none.ref_count), (None, None));
        let none = context_from_text(Mode::R2v, "Ref 10 and v2.5 s");
        assert_eq!((none.duration_s, none.ref_count), (None, None));
    }

    #[test]
    fn request_lengths_skip_ages_decades_and_stray_numbers() {
        let length = |text: &str| context_from_text(Mode::R2v, text).duration_s;
        // Ages and decades before the real length.
        assert_eq!(length("Ref 1: a woman in her late 20s with long hair. 6 s, one take: she sits."), Some(6.0));
        let f2v =
            "Picture 1: a man in his 50s at a desk. Picture 2: he stands at the door. 6 s, one take: he walks out.";
        assert_eq!(context_from_text(Mode::F2v, f2v).duration_s, Some(6.0));
        assert_eq!(
            length("Ref 1: a woman, mid-30s. Style: 90s VHS look, 1990s colours. 8 seconds: she dances."),
            Some(8.0)
        );
        assert_eq!(length("Ref 1: a man in his 30s, shot from the side. Style: 1990s film."), None);
        assert_eq!(length("Ref 1: a woman in her 20s."), None);
        // A decade form right before the take phrase is a length.
        assert_eq!(length("Ref 1: a woman. 20s, one take: she walks."), Some(20.0));
        // Several lengths: the one next to the take / cut phrase wins, else none.
        assert_eq!(length("Ref 1: a woman. She waits 2 seconds, then speaks. 6 s, one take."), Some(6.0));
        assert_eq!(length("Ref 1: a woman. 10 s with 2 cuts: she runs for 3 s, then stops."), Some(10.0));
        assert_eq!(length("Ref 1: a woman. A 10-second clip: she waves."), Some(10.0));
        assert_eq!(length("Ref 1: a woman. Length: 8 s. She waits 2 s."), Some(8.0));
        assert_eq!(length("Ref 1: a woman. 6 s: she waits 2 seconds, then speaks."), None);
        assert_eq!(length("Ref 1: a woman. 6 s: she smiles. In 6 s she turns."), Some(6.0));
        // Stray numbers over 30 s and real-length notes are never the length.
        assert_eq!(length("Ref 1: a 45 s timer on the wall. 4 s: she looks at it."), Some(4.0));
        assert_eq!(length("Ref 1: a woman. 12 s (real length 12.25 s), one take: she sits."), Some(12.0));
        assert_eq!(length("It is really 6.58 s long."), None);
    }

    #[test]
    fn negated_frames_set_no_pin_and_no_last_frame() {
        let r2v = |text: &str| context_from_text(Mode::R2v, text).first_frame_pinned;
        assert_eq!(r2v("Ref 1 is not the first frame: it only gives her look. 6 s."), Some(false));
        assert_eq!(r2v("Ref 1 isn't the first frame. 6 s."), Some(false));
        assert_eq!(r2v("Ref 1: a portrait, not a first frame. 6 s."), Some(false));
        assert_eq!(r2v("Ref 1 (no filter) is the first frame. 6 s."), Some(true));
        let f2v = |text: &str| {
            let c = context_from_text(Mode::F2v, text);
            (c.ref_count, c.has_last_frame)
        };
        assert_eq!(f2v("Picture 1: a man at a desk (first frame only, no last frame). 6 s."), (Some(1), Some(false)));
        assert_eq!(f2v("Picture 1: a man at a desk, without a last frame. 6 s."), (Some(1), Some(false)));
        assert_eq!(f2v("Picture 1: a man. There is no Picture 2. 6 s."), (Some(1), Some(false)));
        assert_eq!(f2v("Picture 1: a man. Picture 2 is the last frame. 6 s."), (Some(2), Some(true)));
        // An r2v request read as f2v (a fix switched the mode): Ref N are not f2v pictures.
        let switched = "Ref 1: a woman. Ref 2: a cafe (place). Use the place from Ref 2. 6 s, one take: she sits.";
        assert_eq!(f2v(switched), (Some(1), Some(false)));
        assert_eq!(context_from_text(Mode::R2v, switched).ref_count, Some(2));
    }

    #[test]
    fn built_requests_give_back_their_facts() {
        for &d in &lengths::DURATIONS {
            for spec in
                [RequestSpec { duration_s: d, ..example() }, RequestSpec { duration_s: d, cuts: 1, ..example() }]
            {
                let text = spec.build_text();
                let real = lengths::format_real(d as f32);
                assert!(text.contains(&format!("\n{d} s (real length {real} s), ")), "{text}");
                let ctx = context_from_text(spec.mode, &text);
                assert_eq!(ctx, spec.lint_context(), "{text}");
            }
            let spec = RequestSpec {
                duration_s: d,
                ..f2v(vec![r(RefRole::FirstFrame, "a man at a desk"), r(RefRole::LastFrame, "he stands")])
            };
            assert_eq!(context_from_text(Mode::F2v, &spec.build_text()), spec.lint_context());
        }
        let twelve = RequestSpec { duration_s: 12, ..example() }.build_text();
        assert!(twelve.contains("\n12 s (real length 12.25 s), one take: "), "{twelve}");
        let twenty = RequestSpec { duration_s: 20, ..example() }.build_text();
        assert!(twenty.contains("\n20 s (real length 20.04 s), one take: "), "{twenty}");
    }

    fn r(role: RefRole, desc: &str) -> RefImage {
        RefImage { role, description: desc.to_string(), ..RefImage::default() }
    }

    fn example() -> RequestSpec {
        RequestSpec {
            refs: vec![
                r(RefRole::Person, "a young woman, long black hair in a high bun, cream crochet one-piece swimsuit"),
                r(RefRole::Place, "an empty sunlit cafe with tall windows."),
            ],
            outfit: "a white linen sundress".into(),
            extra_people: "a young man with short dark hair, blue shirt".into(),
            action: "she walks to the window table and sits down".into(),
            camera: CameraPreset::PushIn,
            dialogue: vec![DialogueLine { speaker: "she".into(), words: "Good morning.".into() }],
            notes: "Keep it gentle.".into(),
            ..RequestSpec::default()
        }
    }

    fn f2v(refs: Vec<RefImage>) -> RequestSpec {
        RequestSpec {
            mode: Mode::F2v,
            refs,
            action: "she walks to the window table and sits".into(),
            ..RequestSpec::default()
        }
    }

    fn has_warning(spec: &RequestSpec, needle: &str) -> bool {
        spec.warnings().iter().any(|w| w.contains(needle))
    }

    /// A unique scratch folder under the system temp dir.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bh3-request-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn r2v_text_matches_the_tested_request_shape() {
        let want = "Ref 1: a young woman, long black hair in a high bun, cream crochet one-piece swimsuit (person).\n\
Ref 2: an empty sunlit cafe with tall windows (place). Use the place from Ref 2.\n\
Outfit: a white linen sundress.\n\
A young man with short dark hair, blue shirt (no photo) is also in the scene.\n\
6 s (real length 6.58 s), one take: she walks to the window table and sits down. Camera: the camera pushes in slowly. Style: film look: 35mm anamorphic, soft grain, warm grade, shallow depth of field.\n\
She says \"Good morning.\"\n\
Keep it gentle.";
        assert_eq!(example().build_text(), want);
        assert!(example().warnings().is_empty(), "{:?}", example().warnings());
    }

    #[test]
    fn empty_fields_are_skipped() {
        let spec = RequestSpec {
            refs: vec![r(RefRole::Person, "a man in a grey suit")],
            action: "he sits down".into(),
            style: StylePreset::Custom,
            ..RequestSpec::default()
        };
        assert_eq!(
            spec.build_text(),
            "Ref 1: a man in a grey suit (person).\n6 s (real length 6.58 s), one take: he sits down."
        );
        let custom = RequestSpec {
            style_custom: "pastel colours, soft haze.".into(),
            camera: CameraPreset::Custom,
            camera_custom: "slow crane up".into(),
            place: "a rooftop at dusk".into(),
            ..spec
        };
        assert_eq!(
            custom.build_text(),
            "Ref 1: a man in a grey suit (person).\nPlace: a rooftop at dusk.\n6 s (real length 6.58 s), one take: he sits down. Camera: slow crane up. Style: pastel colours, soft haze."
        );
    }

    #[test]
    fn f2v_uses_picture_wording() {
        let spec = RequestSpec {
            style: StylePreset::Custom,
            ..f2v(vec![
                r(RefRole::FirstFrame, "a woman in a white blouse at a cafe counter"),
                r(RefRole::LastFrame, "the same woman seated at a window table"),
            ])
        };
        assert_eq!(
            spec.build_text(),
            "Picture 1: a woman in a white blouse at a cafe counter (first frame).\n\
Picture 2: the same woman seated at a window table (last frame).\n\
6 s (real length 6.58 s), one shot: she walks to the window table and sits."
        );
        assert!(spec.has_last_frame());
        assert!(spec.warnings().is_empty(), "{:?}", spec.warnings());
        let ctx = spec.lint_context();
        assert_eq!(ctx.mode, Mode::F2v);
        assert_eq!(ctx.has_last_frame, Some(true));
        assert_eq!(ctx.first_frame_pinned, None);
        assert_eq!(ctx.ref_count, Some(2));
        assert_eq!(ctx.duration_s, Some(6.0));
        // Picture 1 is the first frame whatever its role says (and the role is flagged).
        let unmarked = f2v(vec![r(RefRole::Person, "a woman at a counter")]);
        assert!(unmarked.build_text().starts_with("Picture 1: a woman at a counter (first frame)."));
        assert_eq!(unmarked.lint_context().has_last_frame, Some(false));
    }

    #[test]
    fn cuts_and_pinned_first_frame() {
        let mut spec = example();
        spec.duration_s = 10;
        spec.cuts = 2;
        assert!(spec.build_text().contains("\n10 s (real length 10.13 s), 2 cuts: she walks"));
        spec.cuts = 1;
        assert!(spec.build_text().contains("\n10 s (real length 10.13 s), 1 cut: she walks"));

        let pinned = RequestSpec {
            refs: vec![r(RefRole::FirstFrame, "he sits at an oak table in the cafe, wide shot.")],
            first_frame_pinned: true,
            action: "he picks up a coffee cup and takes a sip".into(),
            ..RequestSpec::default()
        };
        assert!(pinned
            .build_text()
            .starts_with("Ref 1 is the first frame: he sits at an oak table in the cafe, wide shot.\n6 s (real length 6.58 s), one take: "));
        assert!(pinned.warnings().is_empty(), "{:?}", pinned.warnings());
        assert_eq!(pinned.lint_context().first_frame_pinned, Some(true));
    }

    #[test]
    fn roles_never_reorder_refs() {
        let spec = RequestSpec {
            refs: vec![
                r(RefRole::Place, "a sunlit bedroom"),
                r(RefRole::Person, "a young woman with a high bun"),
                r(RefRole::SamePerson, "full body, side view"),
                r(RefRole::Outfit, "a man in a denim jacket"),
                r(RefRole::Style, "a grainy film still"),
                r(RefRole::Prop, "a red umbrella"),
            ],
            action: "she opens the umbrella".into(),
            ..RequestSpec::default()
        };
        let text = spec.build_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Ref 1: a sunlit bedroom (place). Use the place from Ref 1.");
        assert_eq!(lines[1], "Ref 2: a young woman with a high bun (person).");
        assert_eq!(lines[2], "Ref 3: full body, side view (same person as Ref 2, another photo).");
        assert_eq!(lines[3], "Ref 4: a man in a denim jacket (outfit). Use the outfit from Ref 4.");
        assert_eq!(lines[4], "Ref 5: a grainy film still (style only). Only copy the look of Ref 5.");
        assert_eq!(lines[5], "Ref 6: a red umbrella (object).");
        assert_eq!(spec.lint_context().ref_count, Some(6));
    }

    #[test]
    fn empty_descriptions() {
        let sent = RequestSpec {
            refs: vec![RefImage { send_image: true, image_path: Some("ref1.png".into()), ..RefImage::default() }],
            action: "she waves".into(),
            ..RequestSpec::default()
        };
        let text = sent.build_text();
        assert!(
            text.starts_with("Ref 1: (see the picture) (person).\nAttached pictures, in this order: Ref 1.\n"),
            "{text}"
        );
        assert!(!has_warning(&sent, "no description"));
        assert!(has_warning(&sent, "~1,100 tokens"));

        let unsent =
            RequestSpec { refs: vec![RefImage::default()], action: "she waves".into(), ..RequestSpec::default() };
        assert!(unsent.build_text().starts_with("Ref 1: (person).\n"));
        assert!(has_warning(&unsent, "Ref 1 has no description"));
    }

    #[test]
    fn dialogue_and_extra_people() {
        let mut spec = example();
        spec.dialogue = vec![
            DialogueLine { speaker: "the man".into(), words: "You're up early".into() },
            DialogueLine { speaker: "she".into(), words: "I wanted to see you first!".into() },
            DialogueLine { speaker: "She".into(), words: "Stay a \"little\" longer.".into() },
            DialogueLine { speaker: "".into(), words: "   ".into() },
        ];
        spec.extra_people = "two girls in yellow raincoats".into();
        let text = spec.build_text();
        assert!(text.contains("\nThe man says \"You're up early.\"\nShe answers \"I wanted to see you first!\"\nShe adds \"Stay a 'little' longer.\""), "{text}");
        assert!(text.contains("\nTwo girls in yellow raincoats (no photo) are also in the scene.\n"), "{text}");
        spec.extra_people = "a waiter (no photo) brings the coffee".into();
        assert!(spec.build_text().contains("\nA waiter (no photo) brings the coffee.\n"));
        spec.outfit = "she wears his denim jacket".into();
        assert!(spec.build_text().contains("\nShe wears his denim jacket.\n"));
    }

    #[test]
    fn warnings_cover_refs_and_roles() {
        let ten = RequestSpec {
            refs: (0..10).map(|i| r(RefRole::Person, &format!("person {i}"))).collect(),
            action: "they dance".into(),
            ..RequestSpec::default()
        };
        assert!(has_warning(&ten, "at most 9"));
        assert!(!ten.build_text().contains("Ref 10"));
        assert_eq!(ten.lint_context().ref_count, Some(9));

        let three = f2v(vec![r(RefRole::FirstFrame, "a"), r(RefRole::LastFrame, "b"), r(RefRole::LastFrame, "c")]);
        assert!(has_warning(&three, "only a first frame and an optional last frame"));
        assert!(has_warning(&three, "Picture 3: the exact last frame must be Picture 2"));

        let no_first = f2v(vec![r(RefRole::Person, "a woman")]);
        assert!(has_warning(&no_first, "Picture 1 must be the exact first frame"));
        assert!(has_warning(&f2v(Vec::new()), "f2v needs Picture 1"));
        let last_first = f2v(vec![r(RefRole::LastFrame, "a woman"), r(RefRole::FirstFrame, "b")]);
        assert!(has_warning(&last_first, "Picture 1: the exact last frame must be Picture 2"));
        assert!(has_warning(&last_first, "Picture 2 is the last frame"));

        let mut misplaced = example();
        misplaced.refs[1].role = RefRole::FirstFrame;
        assert!(has_warning(&misplaced, "Ref 2: only Ref 1 can be the exact first frame"));
        let mut last_in_r2v = example();
        last_in_r2v.refs[1].role = RefRole::LastFrame;
        assert!(has_warning(&last_in_r2v, "is an f2v role"));

        let mut pin_only = example();
        pin_only.first_frame_pinned = true;
        assert!(has_warning(&pin_only, "First-frame pin is on, but Ref 1 is not marked"));
        let mut role_only = example();
        role_only.refs[0].role = RefRole::FirstFrame;
        assert!(has_warning(&role_only, "turn the First-frame pin on"));
        assert!(has_warning(&RequestSpec { refs: Vec::new(), ..example() }, "at least one reference picture"));
    }

    #[test]
    fn warnings_cover_action_cuts_and_images() {
        assert!(has_warning(&RequestSpec { action: "  ".into(), ..example() }, "action is empty"));
        let busy = RequestSpec { cuts: 3, ..example() };
        assert!(has_warning(&busy, "3 cuts in 6 s"));
        assert!(!has_warning(&RequestSpec { cuts: 2, ..example() }, "cuts in"));
        assert!(!has_warning(&RequestSpec { cuts: 4, duration_s: 15, ..example() }, "cuts in"));
        assert!(has_warning(&RequestSpec { cuts: 5, duration_s: 15, ..example() }, "5 cuts in 15 s"));
        assert!(has_warning(&RequestSpec { duration_s: 0, ..example() }, "duration is 0 s"));

        let mut no_file = example();
        no_file.refs[0].send_image = true;
        assert!(has_warning(&no_file, "Ref 1: \"send to Bonsai\" is on but no image file is chosen"));
        assert!(!has_warning(&no_file, "tokens"));
        let mut two = example();
        for (i, rf) in two.refs.iter_mut().enumerate() {
            rf.send_image = true;
            rf.image_path = Some(format!("ref{i}.png").into());
        }
        assert_eq!(two.sent_image_count(), 2);
        assert!(has_warning(
            &two,
            "2 picture(s) sent to Bonsai: each costs ~1,100 tokens and ~18 s (~2,200 tokens in total)"
        ));
        assert!(two.build_text().contains("\nAttached pictures, in this order: Ref 1, Ref 2.\n"));
    }

    #[test]
    fn lint_context_matches_the_request() {
        let ctx = example().lint_context();
        assert_eq!(ctx.mode, Mode::R2v);
        assert_eq!(ctx.duration_s, Some(6.0));
        assert_eq!(ctx.ref_count, Some(2));
        assert_eq!(ctx.first_frame_pinned, Some(false));
        assert_eq!(ctx.has_last_frame, None);
        assert_eq!(RequestSpec { duration_s: 0, ..example() }.lint_context().duration_s, None);

        // The context fits a prompt Bonsai wrote for this kind of request.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/good_r2v_romance_1.txt");
        let prompt = std::fs::read_to_string(fixture).expect("fixture");
        let report = lint::lint(&prompt, &example().lint_context());
        assert!(report.ok, "{:#?}", report.issues);
    }

    #[test]
    fn images_are_read_for_sent_refs_only() {
        let dir = temp_dir("images");
        let png = dir.join("ref.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).expect("write png");
        let mut spec = example();
        spec.refs[0].image_path = Some(png.clone());
        assert!(spec.images().expect("no images").is_empty(), "send_image is off");
        spec.refs[0].send_image = true;
        spec.refs[1].send_image = true; // no path: skipped (and warned)
        let images = spec.images().expect("one image");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[0].path.as_deref(), Some(png.as_path()));

        spec.refs[1].image_path = Some(dir.join("missing.jpg"));
        let err = spec.images().expect_err("missing file");
        assert!(format!("{err:#}").contains("Ref 2"), "{err:#}");

        // A follow-up keeps the pictures it can still read and lists only those.
        let (read, missing) = spec.readable_images();
        assert_eq!(read.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), vec!["Ref 1"]);
        assert_eq!(missing, vec!["Ref 2"]);
        let text = spec.build_text();
        assert!(text.contains("\nAttached pictures, in this order: Ref 1, Ref 2.\n"), "{text}");
        let fixed = with_attached_line(&text, &["Ref 1".to_string()]);
        assert!(fixed.contains("\nAttached pictures, in this order: Ref 1.\n"), "{fixed}");
        assert_eq!(fixed.lines().count(), text.lines().count());
        let none = with_attached_line(&text, &[]);
        assert!(!none.contains("Attached pictures"), "{none}");
        assert_eq!(none.lines().count() + 1, text.lines().count());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles() {
        assert_eq!(example().title(), "She walks to the window table");
        let from_ref = RequestSpec { action: String::new(), ..example() };
        assert_eq!(from_ref.title(), "A young woman, long black hair");
        assert_eq!(RequestSpec { action: "she smiles.".into(), ..example() }.title(), "She smiles");
        assert_eq!(RequestSpec::default().title(), "Untitled");
        assert_eq!(RequestSpec { refs: Vec::new(), ..RequestSpec::default() }.title(), "Untitled");
    }

    #[test]
    fn helpers() {
        assert_eq!(thousands(5), "5");
        assert_eq!(thousands(1_100), "1,100");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(capitalize("élan vital"), "Élan vital");
        assert_eq!(sentence("done!"), "done!");
        assert_eq!(clean("  text.. "), "text");
        assert_eq!(max_refs(Mode::R2v), MAX_R2V_REFS);
        assert_eq!(max_refs(Mode::F2v), MAX_F2V_REFS);
        for role in RefRole::all() {
            assert!(!role.label().is_empty());
        }
        // The builder does not guess he / she.
        for camera in CameraPreset::all() {
            let words = format!(" {} ", camera.words());
            assert!(!words.contains(" her ") && !words.contains(" his "), "{camera:?}");
        }
        assert_eq!(outfit_sentence("a black tuxedo."), "Outfit: a black tuxedo.");
    }
}
