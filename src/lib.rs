//! `bh3` - core library of **Bonsai H3 Writer**.
//!
//! Drives a local Bonsai 27B (llama.cpp `llama-server` from the Bonsai-demo repo) to write
//! MiniMax H3 video prompts that follow the official prompt guides, and manages the server
//! process (start, stop, idle auto-stop). Shared by the desktop app (`bonsai-h3-writer`)
//! and the command line tool (`bh3`).

pub mod cli;
pub mod client;
pub mod config;
pub mod gen;
pub mod gpu;
pub mod history;
pub mod lengths;
pub mod library;
pub mod lint;
pub mod request;
pub mod server;
pub mod service;
pub mod writer;

pub use gen::{ChatMessage, GenParams, ImageAttachment, Mode, Role, ThinkingMode};

/// Display name of the application.
pub const APP_NAME: &str = "Bonsai H3 Writer";
/// Folder name used under the user's config directory.
pub const APP_ID: &str = "BonsaiH3Writer";
/// Crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Official H3 guide for first-frame / text prompts (T2VA, I2VA, FL2VA, L2VA).
pub const GUIDE_BASE_URL: &str =
    "https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/docs/VIDEO_PROMPT_WRITING_GUIDE_base_en.md";
/// Official H3 guide for reference mode (r2v).
pub const GUIDE_REF_URL: &str =
    "https://huggingface.co/MiniMaxAI/MiniMax-H3/blob/main/docs/VIDEO_PROMPT_WRITING_GUIDE_ref_en.md";
