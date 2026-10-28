//! Generation settings, chat messages and the llama-server request body.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Which kind of H3 prompt is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Reference-to-video: up to 9 reference images (`<Picture 1>`..`<Picture 9>`).
    #[default]
    R2v,
    /// First-frame video (I2VA) or first + last frame (FL2VA).
    F2v,
}

impl Mode {
    pub fn all() -> [Mode; 2] {
        [Mode::R2v, Mode::F2v]
    }
    /// Short id: `r2v` / `f2v`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::R2v => "r2v",
            Mode::F2v => "f2v",
        }
    }
    /// Human label for buttons.
    pub fn label(&self) -> &'static str {
        match self {
            Mode::R2v => "r2v - reference images",
            Mode::F2v => "f2v - first frame",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "r2v" | "ref" | "reference" | "r2va" => Ok(Mode::R2v),
            "f2v" | "i2v" | "i2va" | "fl2va" | "first" | "first-frame" => Ok(Mode::F2v),
            other => bail!("unknown mode '{other}' (use r2v or f2v)"),
        }
    }
}

/// Bonsai's thinking (reasoning) level, set through the chat template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    /// `enable_thinking: false` - no reasoning tokens. Tested best for prompt writing.
    #[default]
    Off,
    /// `reasoning_effort: "low"`.
    Low,
    /// `reasoning_effort: "medium"`.
    Medium,
    /// `reasoning_effort: "xhigh"` (the model's default when thinking is on).
    High,
}

impl ThinkingMode {
    pub fn all() -> [ThinkingMode; 4] {
        [ThinkingMode::Off, ThinkingMode::Low, ThinkingMode::Medium, ThinkingMode::High]
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingMode::Off => "off",
            ThinkingMode::Low => "low",
            ThinkingMode::Medium => "medium",
            ThinkingMode::High => "high",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            ThinkingMode::Off => "Off",
            ThinkingMode::Low => "Low",
            ThinkingMode::Medium => "Medium",
            ThinkingMode::High => "High",
        }
    }
    /// Value for the template's `reasoning_effort`, `None` when thinking is off.
    pub fn reasoning_effort(&self) -> Option<&'static str> {
        match self {
            ThinkingMode::Off => None,
            ThinkingMode::Low => Some("low"),
            ThinkingMode::Medium => Some("medium"),
            ThinkingMode::High => Some("xhigh"),
        }
    }
    /// One-line hint shown next to the setting.
    pub fn hint(&self) -> &'static str {
        match self {
            ThinkingMode::Off => "No reasoning. Fastest; valid prompts in ~10-25 s.",
            ThinkingMode::Low => "Some reasoning. Can still use thousands of tokens.",
            ThinkingMode::Medium => "More reasoning, slower.",
            ThinkingMode::High => "Full reasoning (xhigh). Slowest; may hit the token limit.",
        }
    }
}

impl fmt::Display for ThinkingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThinkingMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" | "false" | "no" => Ok(ThinkingMode::Off),
            "low" => Ok(ThinkingMode::Low),
            "medium" | "med" => Ok(ThinkingMode::Medium),
            "high" | "xhigh" | "max" | "on" | "true" => Ok(ThinkingMode::High),
            other => bail!("unknown thinking mode '{other}' (use off, low, medium or high)"),
        }
    }
}

/// Sampling and thinking settings for one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenParams {
    pub thinking: ThinkingMode,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: i32,
    pub min_p: f32,
    /// Maximum new tokens (reasoning + answer).
    pub max_tokens: u32,
    /// Fixed seed, or `None` for random.
    pub seed: Option<i64>,
    /// Stream tokens as they are generated.
    pub stream: bool,
}

impl Default for GenParams {
    fn default() -> Self {
        Self::prompt_writer()
    }
}

impl GenParams {
    /// Settings tested on the RTX 4060 laptop for writing H3 prompts:
    /// thinking off + temperature 0.4 gave valid prompts every run.
    pub fn prompt_writer() -> Self {
        Self {
            thinking: ThinkingMode::Off,
            temperature: 0.4,
            top_p: 0.95,
            top_k: 20,
            min_p: 0.05,
            max_tokens: 1500,
            seed: None,
            stream: true,
        }
    }

    /// Defaults for free chat.
    pub fn chat() -> Self {
        Self {
            thinking: ThinkingMode::Off,
            temperature: 0.7,
            top_p: 0.95,
            top_k: 20,
            min_p: 0.05,
            max_tokens: 2048,
            seed: None,
            stream: true,
        }
    }

    /// The `chat_template_kwargs` object that switches Bonsai's thinking.
    pub fn chat_template_kwargs(&self) -> Value {
        match self.thinking.reasoning_effort() {
            None => json!({ "enable_thinking": false }),
            Some(effort) => json!({ "enable_thinking": true, "reasoning_effort": effort }),
        }
    }
}

/// Author of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Longest side (px) of a picture sent to Bonsai: [`ImageAttachment::from_file`] scales bigger
/// ones down, so one costs about 1,000 vision tokens at most (~32x32 px per token).
pub const MAX_IMAGE_SIDE: u32 = 1024;
/// JPEG quality of a scaled-down picture.
const SCALED_JPEG_QUALITY: u8 = 90;

/// An image sent to Bonsai's vision input (costs ~1,100 tokens and ~18 s each on the laptop).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub path: Option<PathBuf>,
    pub mime: String,
    /// Base64 of the bytes sent (no data-URI prefix).
    pub base64: String,
}

impl ImageAttachment {
    /// Read an image file (png, jpg, jpeg, webp, gif, bmp). A png / jpg / webp picture whose longest
    /// side is over [`MAX_IMAGE_SIDE`] is scaled down to it (EXIF rotation applied) and sent as
    /// JPEG, or PNG when it has transparency: llama-server on CUDA does not cap image tokens, and a
    /// phone photo would cost ~4,000 of them. Smaller pictures and files that do not decode are
    /// sent as they are.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mime = match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "bmp" => "image/bmp",
            _ => bail!("unsupported image type '{ext}' ({}): use png, jpg, webp, gif or bmp", path.display()),
        };
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let (mime, bytes) = scaled_down(&bytes).unwrap_or((mime, bytes));
        Ok(Self {
            path: Some(path.to_path_buf()),
            mime: mime.to_string(),
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// `data:<mime>;base64,<...>` for the OpenAI-style `image_url` part.
    pub fn data_uri(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.base64)
    }
}

/// `bytes` scaled down to [`MAX_IMAGE_SIDE`] as (mime, encoded bytes); `None` when the picture is
/// small enough (only its header is read) or cannot be decoded or encoded.
fn scaled_down(bytes: &[u8]) -> Option<(&'static str, Vec<u8>)> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    if width.max(height) <= MAX_IMAGE_SIDE {
        return None;
    }
    let orientation = image::ImageDecoder::orientation(&mut decoder).ok();
    let mut picture = image::DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        picture.apply_orientation(orientation);
    }
    let small = picture.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    if small.color().has_alpha() {
        small.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
        Some(("image/png", out))
    } else {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, SCALED_JPEG_QUALITY);
        small.to_rgb8().write_with_encoder(encoder).ok()?;
        Some(("image/jpeg", out))
    }
}

/// One message of a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self { role, content: content.into(), images: Vec::new() }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
    pub fn with_images(mut self, images: Vec<ImageAttachment>) -> Self {
        self.images = images;
        self
    }

    /// The message as llama-server JSON (text, or text + image parts).
    pub fn to_json(&self) -> Value {
        if self.images.is_empty() {
            json!({ "role": self.role.as_str(), "content": self.content })
        } else {
            let mut parts = vec![json!({ "type": "text", "text": self.content })];
            parts.extend(
                self.images.iter().map(|img| json!({ "type": "image_url", "image_url": { "url": img.data_uri() } })),
            );
            json!({ "role": self.role.as_str(), "content": parts })
        }
    }
}

/// Body for `POST /v1/chat/completions`.
pub fn build_chat_body(messages: &[ChatMessage], params: &GenParams, stream: bool) -> Value {
    let mut body = json!({
        "messages": messages.iter().map(ChatMessage::to_json).collect::<Vec<_>>(),
        "stream": stream,
        "temperature": params.temperature,
        "top_p": params.top_p,
        "top_k": params.top_k,
        "min_p": params.min_p,
        "max_tokens": params.max_tokens,
        "chat_template_kwargs": params.chat_template_kwargs(),
    });
    if let Some(seed) = params.seed {
        body["seed"] = json!(seed);
    }
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_off_disables_template_thinking() {
        let p = GenParams::prompt_writer();
        assert_eq!(p.chat_template_kwargs(), json!({ "enable_thinking": false }));
    }

    #[test]
    fn thinking_high_maps_to_xhigh() {
        let p = GenParams { thinking: ThinkingMode::High, ..GenParams::chat() };
        assert_eq!(p.chat_template_kwargs()["reasoning_effort"], "xhigh");
    }

    #[test]
    fn body_has_images_as_parts() {
        let img = ImageAttachment { path: None, mime: "image/png".into(), base64: "AAAA".into() };
        let msgs = vec![ChatMessage::system("s"), ChatMessage::user("u").with_images(vec![img])];
        let body = build_chat_body(&msgs, &GenParams::prompt_writer(), true);
        assert_eq!(body["messages"][1]["content"][1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("seed").is_none());
    }

    #[test]
    fn big_pictures_are_scaled_down_before_sending() {
        let dir = std::env::temp_dir().join(format!("bh3-gen-images-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let decoded = |img: &ImageAttachment| {
            let bytes = base64::engine::general_purpose::STANDARD.decode(&img.base64).expect("base64");
            image::load_from_memory(&bytes).expect("decodes")
        };
        // A big photo without transparency becomes a JPEG with its longest side at the limit.
        let wide = dir.join("wide.png");
        image::RgbImage::from_pixel(2000, 1000, image::Rgb([200, 120, 40])).save(&wide).expect("save");
        let img = ImageAttachment::from_file(&wide).expect("wide");
        assert_eq!(img.mime, "image/jpeg");
        assert_eq!(image::GenericImageView::dimensions(&decoded(&img)), (MAX_IMAGE_SIDE, 512));
        assert_eq!(img.path.as_deref(), Some(wide.as_path()));
        // Transparency is kept (PNG); a tall picture is limited by its height.
        let tall = dir.join("tall.png");
        image::RgbaImage::from_pixel(600, 3000, image::Rgba([0, 0, 0, 0])).save(&tall).expect("save");
        let img = ImageAttachment::from_file(&tall).expect("tall");
        assert_eq!(img.mime, "image/png");
        assert_eq!(image::GenericImageView::dimensions(&decoded(&img)), (205, MAX_IMAGE_SIDE));
        // Small pictures and files that do not decode go as they are.
        let small = dir.join("small.jpg");
        image::RgbImage::from_pixel(800, 600, image::Rgb([10, 20, 30])).save(&small).expect("save");
        let original = std::fs::read(&small).expect("read");
        let img = ImageAttachment::from_file(&small).expect("small");
        assert_eq!(
            (img.mime.as_str(), img.base64.clone()),
            ("image/jpeg", base64::engine::general_purpose::STANDARD.encode(original))
        );
        let broken = dir.join("broken.webp");
        std::fs::write(&broken, b"not a picture").expect("write");
        let img = ImageAttachment::from_file(&broken).expect("broken");
        assert_eq!((img.mime.as_str(), img.base64.as_str()), ("image/webp", "bm90IGEgcGljdHVyZQ=="));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_modes() {
        assert_eq!("F2V".parse::<Mode>().unwrap(), Mode::F2v);
        assert_eq!("xhigh".parse::<ThinkingMode>().unwrap(), ThinkingMode::High);
        assert!("bogus".parse::<Mode>().is_err());
    }
}
