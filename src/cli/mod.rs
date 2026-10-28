//! `bh3` command line interface - built for scripts and LLM agents (stable JSON with `--json`,
//! meaningful exit codes, no prompts). See `docs/CLI.md`.
//!
//! Exit codes: 0 ok, 1 failure (incl. lint errors with `lint`), 2 usage error,
//! 3 Bonsai unavailable (not running and could not be started), 4 generation failed,
//! 130 cancelled with Ctrl+C.
//!
//! Conventions:
//! - Results go to stdout: plain text, or exactly one JSON document with `--json`. Progress and
//!   notes go to stderr (`--quiet` keeps only warnings and errors). With `--json` a failure prints
//!   `{"error": ..., "exit_code": ..., "kind": ...}` on stdout too, so stdout always parses.
//! - Nothing waits for keyboard input: stdin is read only when the command is told to (`--stdin`,
//!   `lint` / `tokens` without a file), and a terminal on stdin is refused.
//! - Bonsai keeps running after `bh3` exits; `--stop-after` stops it when this command started it.
//! - While Bonsai starts or writes, the first Ctrl+C cancels cleanly (exit 130, the request is
//!   aborted on the server); a second Ctrl+C ends `bh3` at once.
//!
//! `bh3 mcp` offers the same functions as MCP tools on stdin/stdout (`mcp.rs`, `docs/MCP.md`).

mod mcp;
mod models;

use std::cell::OnceCell;
use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;

use crate::client::{BonsaiClient, ChatResult, ServerProps, StreamEvent};
use crate::config::{self, AppConfig};
use crate::describe::{self, Described, DescriptionCache};
use crate::gen::{ChatMessage, GenParams, ImageAttachment, Mode, ThinkingMode};
use crate::gpu::{self, GpuInfo, GpuProcess};
use crate::history::{self, HistoryEntry};
use crate::lengths;
use crate::library::{normalize_text, Library, LibraryItem, Template};
use crate::lint::{self, LintContext, LintIssue, LintReport, Severity};
use crate::request::{self, RefRole, RequestSpec};
use crate::server::{self, BonsaiInstall, ServerManager, ServerState, StartOptions, WaitCancelled};
use crate::writer::{self, WriteJob, WriteOutcome};

/// Exit code: success.
pub const EXIT_OK: i32 = 0;
/// Exit code: general failure / lint errors.
pub const EXIT_FAIL: i32 = 1;
/// Exit code: bad arguments.
pub const EXIT_USAGE: i32 = 2;
/// Exit code: Bonsai not running and could not be started.
pub const EXIT_UNAVAILABLE: i32 = 3;
/// Exit code: Bonsai answered with an error / empty answer.
pub const EXIT_GENERATION: i32 = 4;
/// Exit code: cancelled with Ctrl+C (shell convention 128 + SIGINT).
pub const EXIT_CANCELLED: i32 = 130;

/// Start timeout when the config says 0.
const DEFAULT_START_TIMEOUT_SECS: u64 = 300;
/// Entries `history list` shows without `--limit`.
const DEFAULT_HISTORY_LIMIT: usize = 20;
/// Lines `logs` shows without `--lines`.
const DEFAULT_LOG_LINES: usize = 50;
/// Printed by `bh3 agent-guide`.
const AGENT_GUIDE: &str = include_str!("../../docs/AGENT_GUIDE.md");

/// Set by the first Ctrl+C / Ctrl+Break while a command waits for or talks to Bonsai.
static CANCEL: AtomicBool = AtomicBool::new(false);

/// Run the CLI with `args` (including the program name, like `std::env::args()`); returns the exit code.
pub fn run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    // Known before parsing, so usage errors can be reported as JSON too.
    let json = args.iter().skip(1).any(|a| a.to_str() == Some("--json"));
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(err) => return report_clap_error(&err, json),
    };
    let ctx = Ctx::new(&cli.global);
    match panic::catch_unwind(AssertUnwindSafe(|| dispatch(&ctx, cli.command))) {
        Ok(Ok(code)) => code,
        Ok(Err(err)) => report_error(ctx.json, &err),
        Err(_) => report_error(ctx.json, &CliError::fail("internal error: bh3 crashed (see the message above)")),
    }
}

// ---------------------------------------------------------------------------------------------
// Command line definition
// ---------------------------------------------------------------------------------------------

const AFTER_HELP: &str = "\
Exit codes: 0 ok, 1 failure (lint errors), 2 usage, 3 Bonsai unavailable, 4 generation failed, 130 cancelled.
Typical flow: bh3 status --json -> bh3 write --mode r2v --request-file req.txt --json -> bh3 fix --last --fix \"...\" --json
Full reference: docs/CLI.md. Guide for LLM agents: bh3 agent-guide";

const WRITE_EXAMPLES: &str = "\
Examples:
  bh3 write --mode r2v --request \"Ref 1: a young woman with short black hair in a denim jacket. Place: a rainy neon street at night. 6 s, one take: she turns to the camera and smiles.\"
  bh3 write --mode f2v --request-file req.txt --json --stop-after
  bh3 write --spec-file form.json --out prompt.txt --copy
The checker reads the length (\"6 s\"), the pictures (\"Ref 2\" / \"Picture 2\"), \"Ref 1 is the first frame\" and
\"last frame\" from the request; --duration / --refs / --pinned / --last-frame override that.";

const DESCRIBE_EXAMPLES: &str = "\
Examples:
  bh3 describe photo.jpg --role person
  bh3 describe cafe.png --role place --json
  bh3 describe frame1.png --role first-frame --no-cache --stop-after
Bonsai looks at the picture once (~18 s) and writes one paragraph of what it shows for that role. The same picture
and role comes from the cache next time (descriptions folder in the config folder): no Bonsai needed. Use the text as
the Ref's description, or let `bh3 write --spec-file form.json --describe` fill in the form's empty descriptions.
Roles: person, same-person, place, person-and-place, outfit, prop, style, first-frame, last-frame, keyframe, storyboard";

const FIX_EXAMPLES: &str = "\
Examples:
  bh3 fix --last --fix \"Make the detailed_description longer (about 350 words).\"
  bh3 fix --id 20261028-102644-123 --fix \"Remove the music (non_diegetic_music: N/A).\" --json
  bh3 fix --last --from-lint --json
Tested fix texts: bh3 fixes. The fix is saved as a new history entry (its parent_id is the fixed one).";

const LINT_EXAMPLES: &str = "\
Examples:
  bh3 lint --file prompt.txt --mode r2v --duration 6 --refs 2 --pinned=false
  bh3 history show last --field prompt | bh3 lint --json";

/// Write and check MiniMax H3 video prompts with your local Bonsai 27B.
#[derive(Debug, Parser)]
#[command(
    name = "bh3",
    bin_name = "bh3",
    version,
    about = "Write and check MiniMax H3 video prompts with your local Bonsai 27B",
    long_about = "Write and check MiniMax H3 video prompts with your local Bonsai 27B (llama-server of the \
                  Bonsai-demo folder). Uses the settings and the Bonsai folder of the Bonsai H3 Writer app, starts \
                  Bonsai when a prompt needs it, never asks questions and prints stable JSON with --json.",
    arg_required_else_help = true,
    after_help = AFTER_HELP
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    command: Cmd,
}

/// Options every command takes.
#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = "Global options")]
struct GlobalArgs {
    /// Bonsai server URL [default: server_url from the config, http://127.0.0.1:8080]
    #[arg(long, global = true, value_name = "URL")]
    url: Option<String>,
    /// Bonsai-demo folder [default: bonsai_dir from the config, else found automatically]
    #[arg(long, global = true, value_name = "DIR")]
    bonsai_dir: Option<PathBuf>,
    /// Print the result as one JSON document on stdout (failures too: {"error", "exit_code", "kind"})
    #[arg(long, global = true)]
    json: bool,
    /// No progress messages on stderr (warnings and errors are still printed)
    #[arg(short, long, global = true)]
    quiet: bool,
}

#[derive(Debug, Subcommand)]
// Parsed once per run; boxing the argument structs would only add noise.
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Server, install and GPU status (exit 0 when Bonsai is ready, 3 otherwise)
    Status,
    /// Start Bonsai in the background (it keeps running after bh3 exits)
    Start(StartArgs),
    /// Stop Bonsai (the llama-server on its port) to free the GPU, e.g. for ComfyUI
    Stop,
    /// Stop Bonsai, then start it again
    Restart(StartArgs),
    /// Write an H3 prompt from a request (starts Bonsai when needed)
    #[command(after_help = WRITE_EXAMPLES)]
    Write(WriteArgs),
    /// Describe a picture once with Bonsai's vision input (the text for a reference's description)
    #[command(after_help = DESCRIBE_EXAMPLES)]
    Describe(DescribeArgs),
    /// Send a follow-up fix for a saved prompt
    #[command(after_help = FIX_EXAMPLES)]
    Fix(FixArgs),
    /// Check a prompt against the official H3 format (exit 1 when it has errors)
    #[command(after_help = LINT_EXAMPLES)]
    Lint(LintArgs),
    /// Send one chat message to Bonsai
    Chat(ChatArgs),
    /// List the request templates
    Templates(TemplatesArgs),
    /// Print one request template (ID or number from `bh3 templates`)
    Template(TemplateArgs),
    /// List the tested follow-up fixes
    Fixes(FixesArgs),
    /// Print the system prompt Bonsai gets for a mode
    SystemPrompt(ModeArgs),
    /// Print the short H3 prompt guide of a mode
    Guide(ModeArgs),
    /// Saved prompts: list, show, delete, export
    #[command(subcommand)]
    History(HistoryCmd),
    /// H3 clip lengths: frames, real length, cuts and laptop size limit per requested duration
    Lengths(LengthsArgs),
    /// Count tokens with Bonsai's tokenizer (needs Bonsai running)
    Tokens(TokensArgs),
    /// Show or change the settings (shared with the app)
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Find, show or set the Bonsai-demo folder
    #[command(subcommand)]
    Install(InstallCmd),
    /// Model files: list, download, import from a Bonsai-demo folder, use (in place), remove
    #[command(subcommand)]
    Models(models::ModelsCmd),
    /// The llama-server build the app starts: show, download
    #[command(subcommand)]
    Runtime(models::RuntimeCmd),
    /// GPU memory, temperature, load and processes (nvidia-smi)
    Gpu,
    /// The end of the server log
    Logs(LogsArgs),
    /// Print the guide for LLM agents
    AgentGuide,
    /// Run an MCP server on stdin/stdout for AI agents (Claude Code etc.)
    Mcp,
    /// Print the version
    Version,
}

#[derive(Debug, Args)]
struct StartArgs {
    /// Wait until the model is loaded (exit 3 when the start fails or times out)
    #[arg(long)]
    wait: bool,
    /// Seconds to wait with --wait [default: start_timeout_secs from the config, 300]
    #[arg(long, value_name = "SECS", value_parser = parse_timeout)]
    timeout: Option<u64>,
    /// Start even while other programs (ComfyUI?) hold the GPU memory
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("input").required(true).args(["request", "request_file", "spec_file", "stdin"])))]
struct WriteArgs {
    /// Prompt kind: r2v (reference pictures) or f2v (first frame) [required unless --spec-file gives it]
    #[arg(long, value_enum, required_unless_present = "spec_file")]
    mode: Option<ModeArg>,
    /// The request in plain words
    #[arg(long, value_name = "TEXT")]
    request: Option<String>,
    /// Read the request from a text file (UTF-8, or UTF-16 with BOM)
    #[arg(long, value_name = "FILE")]
    request_file: Option<PathBuf>,
    /// A request form as RequestSpec JSON (see docs/CLI.md); gives the mode and the check facts
    #[arg(long, value_name = "FILE")]
    spec_file: Option<PathBuf>,
    /// Read the request from stdin
    #[arg(long)]
    stdin: bool,
    /// Picture for Bonsai's vision input, repeatable in Ref order (~1,100 tokens and ~18 s each)
    #[arg(long = "image", value_name = "PATH")]
    images: Vec<PathBuf>,
    /// With --spec-file: Bonsai first describes the form's pictures that have an image_path but no
    /// description (once each, cached; see `bh3 describe`), so the prompt is written from text
    #[arg(long)]
    describe: bool,
    #[command(flatten)]
    context: ContextArgs,
    #[command(flatten)]
    gen: GenArgs,
    #[command(flatten)]
    prompt: PromptArgs,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Args)]
struct DescribeArgs {
    /// The picture (png, jpg, webp, gif or bmp; big ones are scaled down to 1024 px)
    #[arg(value_name = "IMAGE")]
    image: PathBuf,
    /// What the picture gives the video; decides what Bonsai describes (see the roles below)
    #[arg(long, value_name = "ROLE", default_value = "person", value_parser = parse_role)]
    role: RefRole,
    /// Ask Bonsai again even when this picture and role were described before
    #[arg(long)]
    no_cache: bool,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("entry").required(true).args(["id", "last"])))]
#[command(group(ArgGroup::new("change").required(true).args(["fix", "from_lint"])))]
struct FixArgs {
    /// History id of the prompt to fix (see `bh3 history list`)
    #[arg(long, value_name = "ID")]
    id: Option<String>,
    /// Fix the newest saved prompt
    #[arg(long)]
    last: bool,
    /// What to change, e.g. "Make the detailed_description longer (about 350 words)."
    #[arg(long, value_name = "TEXT")]
    fix: Option<String>,
    /// Ask Bonsai to fix what the prompt check finds (its errors and warnings) instead of --fix TEXT
    #[arg(long)]
    from_lint: bool,
    #[command(flatten)]
    context: ContextArgs,
    #[command(flatten)]
    gen: GenArgs,
    #[command(flatten)]
    prompt: PromptArgs,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Args)]
struct LintArgs {
    /// Prompt kind [default: detected from the text]
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
    /// The prompt file [default: read stdin]
    #[arg(long, value_name = "FILE")]
    file: Option<PathBuf>,
    #[command(flatten)]
    context: ContextArgs,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("input").required(true).args(["message", "message_file", "stdin"])))]
struct ChatArgs {
    /// The message
    #[arg(long, value_name = "TEXT")]
    message: Option<String>,
    /// Read the message from a text file
    #[arg(long, value_name = "FILE")]
    message_file: Option<PathBuf>,
    /// Read the message from stdin
    #[arg(long)]
    stdin: bool,
    /// System prompt: none, r2v, f2v (the H3 prompt writers) or file:PATH
    #[arg(long, value_name = "none|r2v|f2v|file:PATH", default_value = "none", value_parser = parse_system)]
    system: SystemArg,
    /// Picture for Bonsai's vision input, repeatable (~1,100 tokens and ~18 s each)
    #[arg(long = "image", value_name = "PATH")]
    images: Vec<PathBuf>,
    #[command(flatten)]
    gen: GenArgs,
    #[command(flatten)]
    run: RunArgs,
}

/// Facts the prompt checker needs (unknown facts skip their checks).
#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = "Check facts")]
struct ContextArgs {
    /// Requested clip length in seconds (shot times and the FL2VA line are checked against the real length)
    #[arg(long, value_name = "SECONDS", value_parser = parse_seconds)]
    duration: Option<f32>,
    /// Number of pictures: r2v 1-9 (Ref 1..9), f2v 1-2 (first, last frame)
    #[arg(long, value_name = "N", value_parser = parse_refs)]
    refs: Option<usize>,
    /// r2v: Ref 1 is the exact first frame (First-frame pin on); --pinned=false says it is off
    #[arg(long, value_name = "BOOL", num_args = 0..=1, require_equals = true, default_missing_value = "true",
          value_parser = clap::builder::BoolishValueParser::new())]
    pinned: Option<bool>,
    /// f2v: a last frame is used (FL2VA); --last-frame=false says first frame only
    #[arg(long, value_name = "BOOL", num_args = 0..=1, require_equals = true, default_missing_value = "true",
          value_parser = clap::builder::BoolishValueParser::new())]
    last_frame: Option<bool>,
}

/// Generation settings (defaults from the config).
#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = "Generation")]
struct GenArgs {
    /// Thinking level [default: from the config, off]. Off is tested best; never high on an 8 GB GPU
    #[arg(long, value_enum)]
    thinking: Option<ThinkingArg>,
    /// Sampling temperature 0-2 [default: from the config, 0.4 for prompts, 0.7 for chat]
    #[arg(long, value_name = "T", value_parser = parse_temperature)]
    temperature: Option<f32>,
    /// Most new tokens, reasoning included [default: from the config, 1500 for prompts, 2048 for chat]
    #[arg(long, value_name = "N", value_parser = parse_max_tokens)]
    max_tokens: Option<u32>,
    /// Nucleus sampling 0-1
    #[arg(long, value_name = "P", value_parser = parse_unit)]
    top_p: Option<f32>,
    /// Top-k sampling (0 = off)
    #[arg(long, value_name = "K", value_parser = parse_top_k)]
    top_k: Option<i32>,
    /// Min-p sampling 0-1
    #[arg(long, value_name = "P", value_parser = parse_unit)]
    min_p: Option<f32>,
    /// Fixed seed for repeatable answers [default: random]
    #[arg(long, value_name = "N", allow_negative_numbers = true)]
    seed: Option<i64>,
}

/// History, checking and dry-run options of `write` / `fix`.
#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = "Result")]
struct PromptArgs {
    /// Do not save the prompt to the history (then `bh3 fix --last` cannot fix it)
    #[arg(long)]
    no_history: bool,
    /// Exit 1 when the prompt check finds errors
    #[arg(long)]
    strict: bool,
    /// Only print what would be sent to Bonsai (request or fix, check facts, settings); Bonsai is not used
    #[arg(long)]
    dry_run: bool,
}

/// Server and output options of `write` / `fix` / `chat`.
#[derive(Debug, Clone, Default, Args)]
#[command(next_help_heading = "Server and output")]
struct RunArgs {
    /// Do not start Bonsai when it is not running (exit 3 instead)
    #[arg(long)]
    no_auto_start: bool,
    /// Seconds to wait for Bonsai to start [default: start_timeout_secs from the config, 300]
    #[arg(long, value_name = "SECS", value_parser = parse_timeout)]
    timeout: Option<u64>,
    /// Stop Bonsai afterwards if this command started it (frees the GPU for ComfyUI)
    #[arg(long)]
    stop_after: bool,
    /// Print Bonsai's reasoning and answer to stderr while it writes
    #[arg(long)]
    stream: bool,
    /// Also save the result to FILE (UTF-8 with BOM and CRLF, like Save .txt in the app)
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Also copy the result to the clipboard
    #[arg(long)]
    copy: bool,
}

#[derive(Debug, Args)]
struct TemplatesArgs {
    /// Only the templates of this mode
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
}

#[derive(Debug, Args)]
struct FixesArgs {
    /// Only the fixes that suit prompts of this mode [default: all]
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
}

#[derive(Debug, Args)]
struct TemplateArgs {
    /// Template id (e.g. first-frame-only) or its number in `bh3 templates`
    #[arg(value_name = "ID")]
    id: String,
}

#[derive(Debug, Args)]
struct ModeArgs {
    /// r2v or f2v
    #[arg(value_enum)]
    mode: ModeArg,
}

#[derive(Debug, Subcommand)]
enum HistoryCmd {
    /// List saved prompts, newest first
    List(HistoryListArgs),
    /// Show a saved prompt (ID or `last`)
    Show(HistoryShowArgs),
    /// Delete a saved prompt
    Delete(HistoryIdArgs),
    /// Save a prompt as a .txt file (UTF-8 with BOM and CRLF, for Notepad and ComfyUI)
    Export(HistoryExportArgs),
}

#[derive(Debug, Args)]
struct HistoryListArgs {
    /// Show at most N entries (0 = all)
    #[arg(long, short = 'n', value_name = "N", default_value_t = DEFAULT_HISTORY_LIMIT)]
    limit: usize,
    /// Only prompts of this mode
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
    /// Only entries containing all these words (title, request, prompt, id)
    #[arg(long, value_name = "WORDS")]
    search: Option<String>,
    /// Only starred entries
    #[arg(long)]
    starred: bool,
}

#[derive(Debug, Args)]
struct HistoryShowArgs {
    /// History id, or `last` for the newest entry
    #[arg(value_name = "ID|last")]
    id: String,
    /// Print only this part
    #[arg(long, value_enum)]
    field: Option<FieldArg>,
}

#[derive(Debug, Args)]
struct HistoryIdArgs {
    /// History id (see `bh3 history list`)
    #[arg(value_name = "ID")]
    id: String,
}

#[derive(Debug, Args)]
struct HistoryExportArgs {
    /// History id, or `last` for the newest entry
    #[arg(value_name = "ID|last")]
    id: String,
    /// The .txt file to write
    #[arg(value_name = "FILE")]
    file: PathBuf,
}

#[derive(Debug, Args)]
struct LengthsArgs {
    /// Requested durations in seconds [default: 3 4 5 6 8 10 12 15 20]
    #[arg(value_name = "SECONDS", value_parser = parse_seconds)]
    seconds: Vec<f32>,
}

#[derive(Debug, Args)]
struct TokensArgs {
    /// Count the tokens of this file [default: read stdin]
    #[arg(long, value_name = "FILE")]
    file: Option<PathBuf>,
    /// Count the tokens of this text
    #[arg(long, value_name = "TEXT", conflicts_with = "file")]
    text: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Show the settings (all of config.json with --json)
    Show,
    /// Print the path of config.json
    Path,
    /// Print one setting
    Get(ConfigGetArgs),
    /// Change one setting
    Set(ConfigSetArgs),
}

#[derive(Debug, Args)]
struct ConfigGetArgs {
    /// Setting name, e.g. server_url or writer.temperature (see `bh3 config show`)
    #[arg(value_name = "KEY")]
    key: String,
}

#[derive(Debug, Args)]
struct ConfigSetArgs {
    /// Setting name, e.g. auto_start or writer.thinking (see `bh3 config show`)
    #[arg(value_name = "KEY")]
    key: String,
    /// New value (true/false, a number, a path; `none` clears bonsai_dir and the seeds)
    #[arg(value_name = "VALUE", allow_hyphen_values = true)]
    value: String,
}

#[derive(Debug, Subcommand)]
enum InstallCmd {
    /// List the Bonsai-demo folders found on this PC
    Detect,
    /// Use this Bonsai-demo folder (checked, then saved as bonsai_dir)
    Set(InstallSetArgs),
    /// Show the Bonsai-demo folder in use and what was found in it
    Show,
}

#[derive(Debug, Args)]
struct InstallSetArgs {
    /// The Bonsai-demo folder (with scripts\, bin\ and models\)
    #[arg(value_name = "DIR")]
    dir: PathBuf,
}

#[derive(Debug, Args)]
struct LogsArgs {
    /// How many lines from the end
    #[arg(long, short = 'n', value_name = "N", default_value_t = DEFAULT_LOG_LINES)]
    lines: usize,
}

/// `--mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Reference-to-video: up to 9 reference pictures (<Picture 1>..<Picture 9>)
    #[value(aliases = ["ref", "reference", "r2va"])]
    R2v,
    /// First-frame video (I2VA) or first + last frame (FL2VA)
    #[value(aliases = ["i2v", "i2va", "fl2va", "first-frame"])]
    F2v,
}

impl From<ModeArg> for Mode {
    fn from(mode: ModeArg) -> Self {
        match mode {
            ModeArg::R2v => Mode::R2v,
            ModeArg::F2v => Mode::F2v,
        }
    }
}

/// `--thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ThinkingArg {
    /// No reasoning: fastest, valid prompts in ~10-25 s (tested best)
    #[value(aliases = ["none", "false", "no", "0"])]
    Off,
    /// Some reasoning; can still use thousands of tokens
    Low,
    /// More reasoning, slower
    #[value(alias = "med")]
    Medium,
    /// Full reasoning (xhigh): slowest, may hit the token limit
    #[value(aliases = ["xhigh", "max"])]
    High,
}

impl From<ThinkingArg> for ThinkingMode {
    fn from(thinking: ThinkingArg) -> Self {
        match thinking {
            ThinkingArg::Off => ThinkingMode::Off,
            ThinkingArg::Low => ThinkingMode::Low,
            ThinkingArg::Medium => ThinkingMode::Medium,
            ThinkingArg::High => ThinkingMode::High,
        }
    }
}

/// `history show --field`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum FieldArg {
    /// The H3 prompt
    Prompt,
    /// The request (without the fix of a follow-up)
    Request,
    /// The fix of a follow-up (empty for a plain request)
    Fix,
    /// Bonsai's reasoning (empty with thinking off)
    Reasoning,
    /// The whole entry as JSON
    Json,
}

impl FieldArg {
    fn name(self) -> &'static str {
        match self {
            FieldArg::Prompt => "prompt",
            FieldArg::Request => "request",
            FieldArg::Fix => "fix",
            FieldArg::Reasoning => "reasoning",
            FieldArg::Json => "json",
        }
    }
}

/// `chat --system`.
#[derive(Debug, Clone, PartialEq)]
enum SystemArg {
    None,
    Mode(Mode),
    File(PathBuf),
}

// ---------------------------------------------------------------------------------------------
// Value parsers
// ---------------------------------------------------------------------------------------------

fn parse_system(text: &str) -> Result<SystemArg, String> {
    let text = text.trim();
    if let Some(path) = text.strip_prefix("file:") {
        let path = path.trim();
        if path.is_empty() {
            return Err("file: needs a path, e.g. file:my_system_prompt.txt".to_string());
        }
        return Ok(SystemArg::File(PathBuf::from(path)));
    }
    match text.to_ascii_lowercase().as_str() {
        "" | "none" | "off" | "no" => Ok(SystemArg::None),
        other => Mode::from_str(other)
            .map(SystemArg::Mode)
            .map_err(|_| format!("use none, r2v, f2v or file:PATH (not '{other}')")),
    }
}

/// A number in `min..=max`.
fn parse_number<T>(text: &str, min: T, max: T) -> Result<T, String>
where
    T: FromStr + PartialOrd + std::fmt::Display + Copy,
{
    let value: T = text.trim().parse().map_err(|_| format!("'{text}' is not a number"))?;
    if value < min || value > max {
        return Err(format!("{value} is out of range ({min} to {max})"));
    }
    Ok(value)
}

fn parse_seconds(text: &str) -> Result<f32, String> {
    let value = parse_number(text, 0.0f32, 120.0)?;
    if value <= 0.0 {
        return Err("the duration must be more than 0 seconds".to_string());
    }
    Ok(value)
}

fn parse_refs(text: &str) -> Result<usize, String> {
    parse_number(text, 1usize, request::MAX_R2V_REFS)
}

/// `--role`: a [`RefRole::id`] such as `person`, `place` or `first-frame`.
fn parse_role(text: &str) -> Result<RefRole, String> {
    text.parse::<RefRole>().map_err(|e| e.to_string())
}

fn parse_temperature(text: &str) -> Result<f32, String> {
    parse_number(text, 0.0f32, 2.0)
}

fn parse_unit(text: &str) -> Result<f32, String> {
    parse_number(text, 0.0f32, 1.0)
}

fn parse_max_tokens(text: &str) -> Result<u32, String> {
    parse_number(text, 1u32, 131_072)
}

fn parse_top_k(text: &str) -> Result<i32, String> {
    parse_number(text, 0i32, 1000)
}

fn parse_timeout(text: &str) -> Result<u64, String> {
    parse_number(text, 1u64, 86_400)
}

/// `true`/`false` and the usual spellings (`yes`, `on`, `1` ...).
fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "y" | "on" | "1" => Some(true),
        "false" | "no" | "n" | "off" | "0" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Errors and output
// ---------------------------------------------------------------------------------------------

/// A failed command: exit code and message for the user.
#[derive(Debug, Clone, PartialEq)]
struct CliError {
    code: i32,
    message: String,
}

impl CliError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    fn fail(message: impl Into<String>) -> Self {
        Self::new(EXIT_FAIL, message)
    }
    fn usage(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, message)
    }
    fn unavailable(message: impl Into<String>) -> Self {
        Self::new(EXIT_UNAVAILABLE, message)
    }
    fn generation(message: impl Into<String>) -> Self {
        Self::new(EXIT_GENERATION, message)
    }
    fn cancelled(message: impl Into<String>) -> Self {
        Self::new(EXIT_CANCELLED, message)
    }
}

impl From<anyhow::Error> for CliError {
    fn from(err: anyhow::Error) -> Self {
        Self::fail(format!("{err:#}"))
    }
}

/// Exit code of a finished command, or why it failed.
type CliResult = Result<i32, CliError>;

/// Stable name of an exit code (`kind` in JSON errors).
fn exit_kind(code: i32) -> &'static str {
    match code {
        EXIT_OK => "ok",
        EXIT_USAGE => "usage",
        EXIT_UNAVAILABLE => "unavailable",
        EXIT_GENERATION => "generation",
        EXIT_CANCELLED => "cancelled",
        _ => "failure",
    }
}

/// Help and version go to stdout (exit 0); usage errors to stderr (exit 2), and as JSON with `--json`.
fn report_clap_error(err: &clap::Error, json: bool) -> i32 {
    if !err.use_stderr() {
        let _ = err.print();
        return EXIT_OK;
    }
    if json {
        let text = err.render().to_string();
        let message = text.trim().strip_prefix("error:").unwrap_or(text.trim()).trim().to_string();
        print_error_json(&message, EXIT_USAGE);
    }
    let _ = err.print();
    EXIT_USAGE
}

fn report_error(json: bool, err: &CliError) -> i32 {
    if json {
        print_error_json(&err.message, err.code);
    }
    eprint_line(&format!("error: {}", err.message));
    err.code
}

fn print_error_json(message: &str, code: i32) {
    let value = json!({ "error": message, "exit_code": code, "kind": exit_kind(code) });
    print_line(&serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string()));
}

/// Write `text` and a final newline; a closed pipe (`bh3 ... | head`) is not an error.
fn write_line(stream: &mut dyn Write, text: &str) {
    let _ = stream.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = stream.write_all(b"\n");
    }
    let _ = stream.flush();
}

/// A line (or block) of results on stdout.
fn print_line(text: &str) {
    write_line(&mut std::io::stdout().lock(), text);
}

/// A line on stderr.
fn eprint_line(text: &str) {
    write_line(&mut std::io::stderr().lock(), text);
}

/// One JSON document on stdout. Structs are serialised directly (not through `serde_json::Value`)
/// so `f32` settings print as `0.4`, not `0.4000000059604645`.
fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<(), CliError> {
    let text =
        serde_json::to_string_pretty(value).map_err(|e| CliError::fail(format!("encoding the JSON answer: {e}")))?;
    print_line(&text);
    Ok(())
}

fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------------------------
// Run context: config, install, server
// ---------------------------------------------------------------------------------------------

/// Where the Bonsai folder came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum Source {
    /// `--bonsai-dir`.
    #[serde(rename = "option")]
    Flag,
    /// `bonsai_dir` in the config.
    #[serde(rename = "config")]
    Config,
    /// Found automatically ([`server::find_installs`]).
    #[serde(rename = "detected")]
    Detected,
    /// The app's own runtime (`runtime.model` in the config, no Bonsai-demo folder needed).
    #[serde(rename = "runtime")]
    Runtime,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Flag => "from --bonsai-dir",
            Source::Config => "from the config",
            Source::Detected => "found automatically",
            Source::Runtime => "the app's own runtime",
        }
    }
}

/// The Bonsai folder a command uses.
#[derive(Debug, Clone)]
struct Resolved {
    source: Source,
    dir: PathBuf,
    /// What [`BonsaiInstall::detect`] made of it.
    detected: Result<BonsaiInstall, String>,
}

/// Global options plus the lazily loaded config.
struct Ctx {
    json: bool,
    quiet: bool,
    url: Option<String>,
    bonsai_dir: Option<PathBuf>,
    /// Overrides the config's `auto_start` (an MCP tool's `auto_start` argument).
    auto_start: Option<bool>,
    config: OnceCell<AppConfig>,
}

impl Ctx {
    fn new(global: &GlobalArgs) -> Self {
        Self {
            json: global.json,
            quiet: global.quiet,
            url: global.url.as_ref().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()),
            bonsai_dir: global.bonsai_dir.clone().filter(|d| !d.as_os_str().is_empty()),
            auto_start: None,
            config: OnceCell::new(),
        }
    }

    /// The same options with the config read again on first use (the MCP server makes one per
    /// tool call, so changes made in the app apply without a restart).
    fn reloaded(&self) -> Self {
        Self {
            json: self.json,
            quiet: self.quiet,
            url: self.url.clone(),
            bonsai_dir: self.bonsai_dir.clone(),
            auto_start: None,
            config: OnceCell::new(),
        }
    }

    /// The settings (defaults when config.json is missing or unreadable, like the app).
    fn config(&self) -> &AppConfig {
        self.config.get_or_init(AppConfig::load)
    }

    /// Server URL without trailing slash (`--url`, else the config).
    fn url(&self) -> String {
        let raw = self.url.clone().unwrap_or_else(|| self.config().base_url());
        BonsaiClient::new(&raw).base_url().to_string()
    }

    /// A progress line on stderr (not with `--quiet`).
    fn note(&self, text: impl AsRef<str>) {
        if !self.quiet {
            eprint_line(text.as_ref());
        }
    }

    /// A warning on stderr (also with `--quiet`).
    fn warn(&self, text: impl AsRef<str>) {
        eprint_line(&format!("warning: {}", text.as_ref()));
    }

    /// Manager for the server URL; the install only matters for starting.
    fn manager(&self, install: Option<BonsaiInstall>) -> ServerManager {
        ServerManager::new(install.unwrap_or_default(), &self.url())
    }

    /// `--bonsai-dir`, else the app's own runtime when the config has a model, else the config's
    /// `bonsai_dir`, else the first (usable) install found.
    fn resolve_install(&self) -> Option<Resolved> {
        if self.bonsai_dir.is_none() && self.config().runtime.is_active() {
            let install = BonsaiInstall::from_runtime(self.config());
            return Some(Resolved { source: Source::Runtime, dir: install.root.clone(), detected: Ok(install) });
        }
        let chosen = match (&self.bonsai_dir, &self.config().bonsai_dir) {
            (Some(dir), _) => Some((Source::Flag, dir.clone())),
            (None, Some(dir)) if !dir.as_os_str().is_empty() => Some((Source::Config, dir.clone())),
            _ => None,
        };
        if let Some((source, dir)) = chosen {
            let dir = absolute(&dir);
            let detected = BonsaiInstall::detect(&dir).map_err(|e| format!("{e:#}"));
            return Some(Resolved { source, dir, detected });
        }
        let found = server::find_installs();
        let pick = found.iter().find(|i| i.is_usable()).or_else(|| found.first())?.clone();
        Some(Resolved { source: Source::Detected, dir: pick.root.clone(), detected: Ok(pick) })
    }

    /// A usable install for starting Bonsai, or why there is none (exit 3).
    fn startable_install(&self, what: &str) -> Result<BonsaiInstall, CliError> {
        let Some(resolved) = self.resolve_install() else {
            return Err(CliError::unavailable(format!(
                "{what}: no model and no Bonsai-demo folder is set, and none was found. Use model files with \
                 `bh3 models use <file.gguf>` (+ `bh3 runtime download` for the server), or set a Bonsai-demo folder \
                 with `bh3 install set <DIR>` (or pass --bonsai-dir)."
            )));
        };
        let install = resolved
            .detected
            .map_err(|e| CliError::unavailable(format!("{what}: {e} (fix it with `bh3 install set <DIR>`)")))?;
        let problems = install.problems();
        if !problems.is_empty() {
            let from =
                if install.runtime { "the app's runtime".to_string() } else { install.root.display().to_string() };
            return Err(CliError::unavailable(format!(
                "{what}: Bonsai cannot be started from {from}: {}",
                problems.join("; ")
            )));
        }
        if resolved.source == Source::Detected {
            self.note(format!(
                "Using the Bonsai folder {} (found automatically; save it with `bh3 install set`).",
                install.root.display()
            ));
        }
        Ok(install)
    }

    /// How long to wait for a start: the option, else the config (0 there = 300 s).
    fn start_timeout(&self, option: Option<u64>) -> Duration {
        let secs = option.unwrap_or(match self.config().start_timeout_secs {
            0 => DEFAULT_START_TIMEOUT_SECS,
            n => u64::from(n),
        });
        Duration::from_secs(secs)
    }

    /// Bonsai Ready for a generation command: use a running server, wait for one that loads, or
    /// start one (unless auto-start is off).
    fn ensure_ready(&self, run: &RunArgs) -> Result<Session, CliError> {
        let url = self.url();
        let mut mgr = self.manager(None);
        let state = mgr.refresh().clone();
        if state.is_ready() {
            return Ok(Session { mgr, started: false });
        }
        console::install();
        let timeout = self.start_timeout(run.timeout);
        let begun = Instant::now();
        let mut progress = |s: &ServerState| self.note(format!("Bonsai: {}", s.label()));
        if matches!(state, ServerState::Loading | ServerState::Starting) {
            self.note(format!("Bonsai at {url} is loading the model; waiting up to {} s...", timeout.as_secs()));
            mgr.wait_ready(timeout, &CANCEL, &mut progress).map_err(|e| start_error(&e))?;
            self.note(format!("Bonsai is ready ({}).", fmt_elapsed(begun.elapsed())));
            return Ok(Session { mgr, started: false });
        }
        let cfg = self.config();
        if run.no_auto_start || !self.auto_start.unwrap_or(cfg.auto_start) {
            let why = match (run.no_auto_start, self.auto_start) {
                (true, _) => "--no-auto-start",
                (false, Some(false)) => "auto_start false",
                _ => "auto_start is off in the settings",
            };
            return Err(CliError::unavailable(format!(
                "Bonsai is not running at {url} ({why}). Start it with `bh3 start --wait`."
            )));
        }
        let install = self.startable_install("Bonsai is not running")?;
        self.note(format!(
            "Starting Bonsai from {} (loading the model takes about a minute)...",
            install.root.display()
        ));
        let mut mgr = ServerManager::new(install, &url);
        match mgr.ensure_running(&StartOptions::from_config(cfg), timeout, &CANCEL, &mut progress) {
            Ok(started) => {
                self.note(format!("Bonsai is ready ({}).", fmt_elapsed(begun.elapsed())));
                Ok(Session { mgr, started })
            }
            // A start that timed out or was cancelled may still be loading: --stop-after ends it.
            Err(e) if run.stop_after && mgr.started_by_us() => {
                self.note("Stopping the Bonsai this command started (--stop-after)...");
                let stop = mgr.stop();
                if let Err(stop_err) = &stop {
                    self.warn(format!("could not stop Bonsai: {stop_err:#}"));
                }
                let mut err = start_error(&e);
                if stop.is_ok() {
                    err.message = if err.code == EXIT_CANCELLED {
                        "cancelled (Ctrl+C); the Bonsai this command started was stopped (--stop-after)".to_string()
                    } else {
                        format!("{} (the Bonsai this command started was stopped: --stop-after)", err.message)
                    };
                }
                Err(err)
            }
            Err(e) => Err(start_error(&e)),
        }
    }

    /// Stream one chat request (progress or `--stream` text on stderr).
    fn generate(
        &self,
        session: &mut Session,
        messages: &[ChatMessage],
        params: &GenParams,
        stream: bool,
    ) -> Result<ChatResult, CliError> {
        console::install();
        let mut printer = StreamPrinter::new(self.quiet, stream);
        let result = session.mgr.client().chat_stream(messages, params, &CANCEL, &mut |piece| printer.piece(&piece));
        printer.finish();
        session.mgr.touch();
        let aborted = "cancelled (Ctrl+C): the request to Bonsai was aborted";
        match result {
            Err(_) if cancelled() => Err(CliError::cancelled(aborted)),
            Err(e) => {
                let message = format!("Request to Bonsai failed: {e:#}");
                // A fresh check (a manager tolerates one missed health check while Ready).
                if self.manager(None).refresh().is_ready() {
                    Err(CliError::generation(message))
                } else {
                    Err(CliError::unavailable(message))
                }
            }
            Ok(result) if result.cancelled => Err(CliError::cancelled(aborted)),
            Ok(result) => Ok(result),
        }
    }

    /// After a generation command: stop Bonsai when this command started it and `--stop-after`
    /// asks for it. Returns true when it was stopped; a failed stop is printed and added to `warnings`.
    fn end_session(&self, session: &mut Session, stop_after: bool, warnings: &mut Vec<String>) -> bool {
        if !session.started || !session.mgr.started_by_us() || !stop_after {
            return false;
        }
        self.note("Stopping Bonsai (this command started it)...");
        match session.mgr.stop() {
            Ok(()) => {
                self.note("Bonsai stopped: the GPU is free for ComfyUI.");
                true
            }
            Err(e) => {
                let text = format!("could not stop Bonsai: {e:#}");
                self.warn(&text);
                warnings.push(text);
                false
            }
        }
    }
}

/// The server a generation command uses.
struct Session {
    mgr: ServerManager,
    /// This command started it.
    started: bool,
}

impl Session {
    /// The last note of a command that started Bonsai and leaves it running.
    fn keep_running_note(&self, ctx: &Ctx, stop_after: bool) {
        if self.started && !stop_after {
            ctx.note("Bonsai keeps running (this command started it): `bh3 stop` frees the GPU for ComfyUI.");
        }
    }
}

/// Error of a failed or cancelled start / wait (exit 3, or 130 for Ctrl+C).
fn start_error(err: &anyhow::Error) -> CliError {
    if err.downcast_ref::<WaitCancelled>().is_some() || cancelled() {
        return CliError::cancelled("cancelled (Ctrl+C): stopped waiting; Bonsai keeps starting in the background");
    }
    let message = format!("{err:#}");
    if message.contains("Not starting Bonsai") {
        return CliError::unavailable(format!(
            "{message}\nClose the other program, or start Bonsai anyway with `bh3 start --force --wait` and run \
             this command again."
        ));
    }
    CliError::unavailable(message)
}

/// Streamed pieces: phase notes (`Bonsai is thinking...`), or the raw text with `--stream`.
struct StreamPrinter {
    quiet: bool,
    stream: bool,
    /// Last phase announced (notes only move forward: thinking -> writing).
    phase: Option<PieceKind>,
    /// Kind of the text being streamed (a header is printed when it changes).
    printing: Option<PieceKind>,
    at_line_start: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PieceKind {
    Reasoning,
    Content,
}

impl StreamPrinter {
    fn new(quiet: bool, stream: bool) -> Self {
        Self { quiet, stream, phase: None, printing: None, at_line_start: true }
    }

    fn piece(&mut self, piece: &StreamEvent) {
        let (kind, text) = match piece {
            StreamEvent::Reasoning(text) => (PieceKind::Reasoning, text.as_str()),
            StreamEvent::Content(text) => (PieceKind::Content, text.as_str()),
        };
        if self.stream {
            let mut stderr = std::io::stderr().lock();
            if self.printing != Some(kind) {
                if text.trim().is_empty() {
                    return; // whitespace between reasoning and answer
                }
                if !self.at_line_start {
                    let _ = stderr.write_all(b"\n");
                }
                let header = match kind {
                    PieceKind::Reasoning => "--- reasoning ---",
                    PieceKind::Content => "--- answer ---",
                };
                let _ = writeln!(stderr, "{header}");
                self.printing = Some(kind);
            }
            let _ = stderr.write_all(text.as_bytes());
            let _ = stderr.flush();
            if !text.is_empty() {
                self.at_line_start = text.ends_with('\n');
            }
            return;
        }
        if self.quiet || text.trim().is_empty() {
            return;
        }
        let advance = match self.phase {
            None => true,
            Some(PieceKind::Reasoning) => kind == PieceKind::Content,
            Some(PieceKind::Content) => false,
        };
        if advance {
            self.phase = Some(kind);
            eprint_line(match kind {
                PieceKind::Reasoning => "Bonsai is thinking...",
                PieceKind::Content => "Bonsai is writing...",
            });
        }
    }

    fn finish(&mut self) {
        if self.stream && !self.at_line_start {
            eprint_line("");
            self.at_line_start = true;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------------------------

fn dispatch(ctx: &Ctx, command: Cmd) -> CliResult {
    match command {
        Cmd::Status => cmd_status(ctx),
        Cmd::Start(args) => cmd_start(ctx, &args, false),
        Cmd::Stop => cmd_stop(ctx),
        Cmd::Restart(args) => cmd_start(ctx, &args, true),
        Cmd::Write(args) => cmd_write(ctx, &args),
        Cmd::Describe(args) => cmd_describe(ctx, &args),
        Cmd::Fix(args) => cmd_fix(ctx, &args),
        Cmd::Lint(args) => cmd_lint(ctx, &args),
        Cmd::Chat(args) => cmd_chat(ctx, &args),
        Cmd::Templates(args) => cmd_templates(ctx, &args),
        Cmd::Template(args) => cmd_template(ctx, &args),
        Cmd::Fixes(args) => cmd_fixes(ctx, &args),
        Cmd::SystemPrompt(args) => cmd_library_text(ctx, args.mode.into(), false),
        Cmd::Guide(args) => cmd_library_text(ctx, args.mode.into(), true),
        Cmd::History(cmd) => cmd_history(ctx, &cmd),
        Cmd::Lengths(args) => cmd_lengths(ctx, &args),
        Cmd::Tokens(args) => cmd_tokens(ctx, &args),
        Cmd::Config(cmd) => cmd_config(ctx, &cmd),
        Cmd::Install(cmd) => cmd_install(ctx, &cmd),
        Cmd::Models(cmd) => models::cmd_models(ctx, &cmd),
        Cmd::Runtime(cmd) => models::cmd_runtime(ctx, &cmd),
        Cmd::Gpu => cmd_gpu(ctx),
        Cmd::Logs(args) => cmd_logs(ctx, &args),
        Cmd::AgentGuide => cmd_agent_guide(ctx),
        Cmd::Mcp => Ok(mcp::serve(ctx)),
        Cmd::Version => cmd_version(ctx),
    }
}

// ---------------------------------------------------------------------------------------------
// status / start / stop / restart
// ---------------------------------------------------------------------------------------------

/// `state` in JSON.
fn state_id(state: &ServerState) -> &'static str {
    match state {
        ServerState::Stopped => "stopped",
        ServerState::Starting => "starting",
        ServerState::Loading => "loading",
        ServerState::Ready => "ready",
        ServerState::Stopping => "stopping",
        ServerState::Failed(_) => "failed",
    }
}

#[derive(Debug, Serialize)]
struct InstallJson {
    root: PathBuf,
    source: Option<Source>,
    usable: bool,
    problems: Vec<String>,
    warnings: Vec<String>,
    launcher: Option<PathBuf>,
    server_exe: Option<PathBuf>,
    backend: Option<&'static str>,
    model: Option<PathBuf>,
    models: Vec<PathBuf>,
    mmproj: Option<PathBuf>,
}

impl InstallJson {
    fn of(install: &BonsaiInstall, source: Option<Source>) -> Self {
        Self {
            root: install.root.clone(),
            source,
            usable: install.is_usable(),
            problems: install.problems(),
            warnings: install.warnings(),
            launcher: install.launcher_ps1.clone(),
            server_exe: install.server_exe.clone(),
            backend: install.backend(),
            model: install.primary_model().cloned(),
            models: install.models.clone(),
            mmproj: install.mmproj.clone(),
        }
    }

    fn of_resolved(resolved: &Resolved) -> Self {
        match &resolved.detected {
            Ok(install) => Self::of(install, Some(resolved.source)),
            Err(problem) => Self {
                root: resolved.dir.clone(),
                source: Some(resolved.source),
                usable: false,
                problems: vec![problem.clone()],
                warnings: Vec::new(),
                launcher: None,
                server_exe: None,
                backend: None,
                model: None,
                models: Vec::new(),
                mmproj: None,
            },
        }
    }

    /// `D:\Bonsai-demo (CUDA, usable, from the config)` + one line per problem.
    fn line(&self) -> String {
        let mut facts: Vec<&str> = Vec::new();
        if let Some(backend) = self.backend {
            facts.push(backend);
        }
        facts.push(if self.usable { "usable" } else { "not usable" });
        if let Some(source) = self.source {
            facts.push(source.label());
        }
        let mut line = format!("{} ({})", self.root.display(), facts.join(", "));
        for problem in &self.problems {
            line.push_str(&format!("\n{:9}problem: {problem}", ""));
        }
        line
    }
}

#[derive(Debug, Serialize)]
struct GpuJson {
    #[serde(flatten)]
    info: GpuInfo,
    mem_free_mb: u32,
    /// VRAM used by programs other than llama-server.
    vram_used_by_others_mb: u32,
    other_processes: Vec<GpuProcess>,
    /// The auto-start refuses above this (`vram_busy_mb`, 0 = never).
    vram_busy_mb: u32,
    /// Why a non-forced start would be refused now.
    busy: Option<String>,
}

impl GpuJson {
    fn new(info: GpuInfo, vram_busy_mb: u32) -> Self {
        Self {
            mem_free_mb: info.mem_total_mb.saturating_sub(info.mem_used_mb),
            vram_used_by_others_mb: gpu::vram_used_by_others(&info),
            other_processes: gpu::other_processes(&info),
            busy: gpu::vram_busy_message(&info, vram_busy_mb),
            vram_busy_mb,
            info,
        }
    }

    /// Usage line, one line per process, and (with `start_hint`) whether a start would be refused.
    fn lines(&self, start_hint: bool) -> Vec<String> {
        let info = &self.info;
        let mut lines = vec![format!(
            "{}: {} of {} used ({:.0}%), {} °C, load {}%",
            info.name,
            gpu::fmt_mb(info.mem_used_mb),
            gpu::fmt_mb(info.mem_total_mb),
            info.mem_fraction() * 100.0,
            info.temp_c,
            info.util_pct
        )];
        for p in &info.processes {
            lines.push(format!("PID {:<7} {}", p.pid, gpu::describe_process(p)));
        }
        if !start_hint {
            return lines;
        }
        match &self.busy {
            Some(message) => lines.push(format!("Busy: {message}")),
            None => lines.push(format!(
                "Other programs use {} (auto-start limit {}): Bonsai can start",
                gpu::fmt_mb(self.vram_used_by_others_mb),
                if self.vram_busy_mb == 0 { "off".to_string() } else { format!("{} MB", self.vram_busy_mb) }
            )),
        }
        lines
    }
}

#[derive(Debug, Serialize)]
struct StatusJson {
    state: &'static str,
    state_label: String,
    ready: bool,
    url: String,
    message: Option<String>,
    llama_server_pids: Vec<u32>,
    install: Option<InstallJson>,
    props: Option<ServerProps>,
    gpu: Option<GpuJson>,
}

fn cmd_status(ctx: &Ctx) -> CliResult {
    let status = status_report(ctx);
    if ctx.json {
        print_json(&status)?;
    } else {
        print_line(&status_text(&status));
    }
    Ok(if status.ready { EXIT_OK } else { EXIT_UNAVAILABLE })
}

/// Server, install and GPU facts (`status`, and the MCP tool `bonsai_status`).
fn status_report(ctx: &Ctx) -> StatusJson {
    let resolved = ctx.resolve_install();
    let install = resolved.as_ref().and_then(|r| r.detected.as_ref().ok().cloned());
    let mut mgr = ctx.manager(install);
    let state = mgr.refresh().clone();
    let url = mgr.client().base_url().to_string();
    let props = if state.is_ready() { mgr.client().props().ok() } else { None };
    let pids = server::llama_server_pids();
    let message = match &state {
        ServerState::Ready => None,
        ServerState::Loading => Some("Bonsai is loading the model.".to_string()),
        ServerState::Failed(reason) => Some(reason.clone()),
        _ if !pids.is_empty() => Some(format!(
            "llama-server.exe is running (PID {}) but does not answer on {url}: it may still be starting, or it \
             serves another address.",
            join_display(&pids, ", ")
        )),
        _ => Some(format!(
            "Nothing answers on {url}. `bh3 write` starts Bonsai when needed; `bh3 start --wait` starts it now."
        )),
    };
    StatusJson {
        state: state_id(&state),
        state_label: state.label(),
        ready: state.is_ready(),
        url,
        message,
        llama_server_pids: pids,
        install: resolved.as_ref().map(InstallJson::of_resolved),
        props,
        gpu: gpu::query().map(|info| GpuJson::new(info, ctx.config().vram_busy_mb)),
    }
}

fn status_text(status: &StatusJson) -> String {
    let mut lines = vec![format!("{:<9}{} - {}", "Bonsai", status.state_label, status.url)];
    if let Some(message) = &status.message {
        lines.push(format!("{:<9}{message}", ""));
    }
    if let Some(props) = &status.props {
        let name = if props.model_name.is_empty() { props.model_path.as_str() } else { props.model_name.as_str() };
        let ftype = if props.model_ftype.is_empty() { String::new() } else { format!(" ({})", props.model_ftype) };
        lines.push(format!("{:<9}{name}{ftype}", "Model"));
        lines.push(format!(
            "{:<9}context {}, {}, vision {}, build {}",
            "Server",
            props.n_ctx,
            plural(props.total_slots as usize, "slot"),
            if props.vision { "on" } else { "off" },
            if props.build_info.is_empty() { "?" } else { props.build_info.as_str() }
        ));
    }
    match &status.install {
        Some(install) => lines.push(format!("{:<9}{}", "Install", install.line())),
        None => lines.push(format!(
            "{:<9}no model and no Bonsai-demo folder set or found (bh3 models use <file.gguf>, or bh3 install set <DIR>)",
            "Install"
        )),
    }
    match &status.gpu {
        Some(gpu) => {
            // Whether a start would be refused only matters while no llama-server runs (a running
            // one holds the memory itself), like the app's Server workspace.
            let start_hint = !status.ready && status.llama_server_pids.is_empty();
            for (i, line) in gpu.lines(start_hint).into_iter().enumerate() {
                lines.push(format!("{:<9}{line}", if i == 0 { "GPU" } else { "" }));
            }
        }
        None => lines.push(format!("{:<9}no NVIDIA GPU status (nvidia-smi missing?)", "GPU")),
    }
    lines.join("\n")
}

#[derive(Debug, Serialize)]
struct ServerActionJson {
    action: &'static str,
    state: &'static str,
    state_label: String,
    ready: bool,
    url: String,
    /// This command started Bonsai.
    started: bool,
    /// This command stopped Bonsai.
    stopped: bool,
    pid: Option<u32>,
    log: Option<PathBuf>,
    install: Option<PathBuf>,
    message: String,
}

impl ServerActionJson {
    fn new(action: &'static str, state: &ServerState, url: &str, message: impl Into<String>) -> Self {
        Self {
            action,
            state: state_id(state),
            state_label: state.label(),
            ready: state.is_ready(),
            url: url.to_string(),
            started: false,
            stopped: false,
            pid: None,
            log: None,
            install: None,
            message: message.into(),
        }
    }
}

fn print_action(ctx: &Ctx, action: &ServerActionJson) -> Result<(), CliError> {
    if ctx.json {
        print_json(action)
    } else {
        print_line(&action.message);
        Ok(())
    }
}

/// `start` and `restart`.
fn cmd_start(ctx: &Ctx, args: &StartArgs, restart: bool) -> CliResult {
    print_action(ctx, &start_server(ctx, args, restart)?)?;
    Ok(EXIT_OK)
}

/// Start (or restart) Bonsai; what happened, or why it failed (`start`, `restart`, MCP `start_bonsai`).
fn start_server(ctx: &Ctx, args: &StartArgs, restart: bool) -> Result<ServerActionJson, CliError> {
    let name = if restart { "restart" } else { "start" };
    let url = ctx.url();
    let timeout = ctx.start_timeout(args.timeout);
    let begun = Instant::now();
    if args.wait {
        console::install();
    }
    let mut stopped = false;
    let mut probe = ctx.manager(None);
    let state = probe.refresh().clone();
    if restart {
        if state.is_ready() || state == ServerState::Loading || !probe.server_pids().is_empty() {
            ctx.note("Stopping Bonsai...");
            probe.stop().map_err(|e| CliError::fail(format!("could not stop Bonsai: {e:#}")))?;
            stopped = true;
        }
    } else if state.is_ready() {
        return Ok(ServerActionJson::new(name, &state, &url, format!("Bonsai is already running at {url}.")));
    } else if state == ServerState::Loading {
        if !args.wait {
            let message = format!("Bonsai at {url} is already starting (loading the model). Check with `bh3 status`.");
            return Ok(ServerActionJson::new(name, &state, &url, message));
        }
        ctx.note(format!("Bonsai at {url} is already loading the model; waiting up to {} s...", timeout.as_secs()));
        let mut progress = |s: &ServerState| ctx.note(format!("Bonsai: {}", s.label()));
        probe.wait_ready(timeout, &CANCEL, &mut progress).map_err(|e| start_error(&e))?;
        let message = format!("Bonsai is ready at {url} ({}).", fmt_elapsed(begun.elapsed()));
        return Ok(ServerActionJson::new(name, probe.state(), &url, message));
    }

    let install = ctx.startable_install("Cannot start Bonsai")?;
    let root = install.root.clone();
    let mut mgr = ServerManager::new(install, &url);
    let opts = StartOptions { force: args.force, ..StartOptions::from_config(ctx.config()) };
    mgr.start(&opts).map_err(|e| start_error(&e))?;
    let pid = mgr.child_pid();
    let log = mgr.log_path().to_path_buf();
    ctx.note(format!("Starting Bonsai from {} (log: {})...", root.display(), log.display()));

    let message = if args.wait {
        let mut progress = |s: &ServerState| ctx.note(format!("Bonsai: {}", s.label()));
        mgr.wait_ready(timeout, &CANCEL, &mut progress).map_err(|e| start_error(&e))?;
        format!("Bonsai is ready at {url} ({}).", fmt_elapsed(begun.elapsed()))
    } else {
        format!(
            "Bonsai is starting at {url}: loading the model takes about a minute. Check with `bh3 status` \
             (or use --wait)."
        )
    };
    let mut action = ServerActionJson::new(name, mgr.state(), &url, message);
    action.started = true;
    action.stopped = stopped;
    action.pid = pid;
    action.log = Some(log);
    action.install = Some(root);
    Ok(action)
}

fn cmd_stop(ctx: &Ctx) -> CliResult {
    print_action(ctx, &stop_server(ctx)?)?;
    Ok(EXIT_OK)
}

/// Stop the Bonsai on the server URL's port (`stop`, MCP `stop_bonsai`).
fn stop_server(ctx: &Ctx) -> Result<ServerActionJson, CliError> {
    let url = ctx.url();
    let mut mgr = ctx.manager(None);
    let state = mgr.refresh().clone();
    let pids = mgr.server_pids();
    if !state.is_ready() && state != ServerState::Loading && pids.is_empty() {
        return Ok(ServerActionJson::new("stop", &state, &url, "Bonsai is not running."));
    }
    ctx.note("Stopping Bonsai...");
    mgr.stop().map_err(|e| CliError::fail(format!("could not stop Bonsai: {e:#}")))?;
    let mut action = ServerActionJson::new("stop", mgr.state(), &url, "Bonsai stopped: the GPU is free for ComfyUI.");
    action.stopped = true;
    Ok(action)
}

// ---------------------------------------------------------------------------------------------
// write / fix
// ---------------------------------------------------------------------------------------------

/// A [`LintReport`] with error / warning / info counts.
#[derive(Debug, Serialize)]
struct LintJson<'a> {
    #[serde(flatten)]
    report: &'a LintReport,
    errors: usize,
    warnings: usize,
    infos: usize,
}

impl<'a> LintJson<'a> {
    fn new(report: &'a LintReport) -> Self {
        let infos = report.issues.iter().filter(|i| i.severity == Severity::Info).count();
        Self { report, errors: report.errors(), warnings: report.warnings(), infos }
    }
}

/// `write --json` / `fix --json`: the serde form of [`WriteOutcome`] plus the mode and run facts.
#[derive(Debug, Serialize)]
struct WriteJson<'a> {
    mode: Mode,
    prompt: &'a str,
    reasoning: &'a str,
    lint: LintJson<'a>,
    lint_context: &'a LintContext,
    result: &'a ChatResult,
    history_id: Option<&'a str>,
    parent_id: Option<&'a str>,
    started_server: bool,
    stopped_server: bool,
    out_file: Option<&'a Path>,
    copied: bool,
    warnings: &'a [String],
}

fn cmd_write(ctx: &Ctx, args: &WriteArgs) -> CliResult {
    if args.describe && args.spec_file.is_none() {
        return Err(CliError::usage(
            "--describe needs --spec-file (it fills the form's empty descriptions); for a typed request run              `bh3 describe <picture>` first and put its text into the request",
        ));
    }
    let cfg = ctx.config();
    let params = args.gen.apply(cfg.writer_params.clone());
    let mut session = None;
    let mut job = if let Some(path) = &args.spec_file {
        let mut spec = read_spec(path)?;
        if let Some(mode) = args.mode {
            spec.mode = mode.into();
        }
        if args.describe {
            describe_refs(ctx, &mut spec, &args.run, args.prompt.dry_run, &mut session)?;
        }
        for warning in spec.warnings() {
            ctx.note(format!("note: {warning}"));
        }
        WriteJob::from_spec(&spec, params).map_err(CliError::from)?
    } else {
        let mode: Mode = args.mode.map(Into::into).ok_or_else(|| CliError::usage("--mode r2v|f2v is required"))?;
        let text = match (&args.request, &args.request_file) {
            (Some(text), _) => normalize_text(text),
            (None, Some(path)) => read_text_file(path)?,
            (None, None) => read_stdin_text("the request")?,
        };
        text_job(mode, &text, params)?
    };
    job.lint_ctx = args.context.apply(job.lint_ctx.clone())?;
    job.save_history = cfg.save_history && !args.prompt.no_history;
    for path in &args.images {
        job.images.push(ImageAttachment::from_file(path).map_err(CliError::from)?);
    }
    run_prompt_job(ctx, &job, &args.run, &args.prompt, Vec::new(), session)
}

/// `write --describe`: the used refs with a picture, no description and "send to Bonsai" off get
/// Bonsai's description (cached ones without Bonsai; with `dry_run` only those). `session` keeps
/// a Bonsai this started for the write.
fn describe_refs(
    ctx: &Ctx,
    spec: &mut RequestSpec,
    run: &RunArgs,
    dry_run: bool,
    session: &mut Option<Session>,
) -> Result<(), CliError> {
    let mode = spec.mode;
    let used = spec.refs.len().min(request::max_refs(mode));
    let cache = DescriptionCache::open();
    for (i, r) in spec.refs.iter_mut().take(used).enumerate() {
        let Some(path) = r.image_path.clone().filter(|_| !r.send_image && r.description.trim().is_empty()) else {
            continue;
        };
        let name = match mode {
            Mode::R2v => format!("Ref {}", i + 1),
            Mode::F2v => format!("Picture {}", i + 1),
        };
        let described = if dry_run {
            match cache.lookup(&path, r.role) {
                Some(hit) => hit,
                None => {
                    ctx.note(format!("{name}: not described yet (--dry-run does not ask Bonsai)."));
                    continue;
                }
            }
        } else {
            describe_picture(ctx, &path, r.role, true, run, session)
                .map_err(|e| CliError { message: format!("{name}: {}", e.message), ..e })?
        };
        ctx.note(format!("{name}: {}", described.description));
        describe::apply(r, &described.description, "");
    }
    Ok(())
}

/// The picture of `describe`, as an absolute path (missing or not a picture: usage error).
fn check_picture(path: &Path) -> Result<PathBuf, CliError> {
    if !path.is_file() {
        return Err(CliError::usage(format!("the picture {} does not exist", path.display())));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !["png", "jpg", "jpeg", "webp", "gif", "bmp"].contains(&ext.as_str()) {
        return Err(CliError::usage(format!(
            "{} is not a picture Bonsai can read: use png, jpg, webp, gif or bmp",
            path.display()
        )));
    }
    Ok(absolute(path))
}

/// Describe one picture for `role`: from the cache (with `use_cache`), else by Bonsai (made ready
/// when needed and kept in `session`); a new description is cached.
fn describe_picture(
    ctx: &Ctx,
    path: &Path,
    role: RefRole,
    use_cache: bool,
    run: &RunArgs,
    session: &mut Option<Session>,
) -> Result<Described, CliError> {
    let path = check_picture(path)?;
    let cache = DescriptionCache::open();
    if use_cache {
        if let Some(hit) = cache.lookup(&path, role) {
            ctx.note(format!(
                "{} ({}) was described before: from the cache (--no-cache asks Bonsai again).",
                file_label(&path),
                role.id()
            ));
            return Ok(hit);
        }
    }
    let image = ImageAttachment::from_file(&path).map_err(|e| CliError::usage(format!("{e:#}")))?;
    let session = match session {
        Some(session) => session,
        None => session.insert(ctx.ensure_ready(run)?),
    };
    let props = session
        .mgr
        .client()
        .props()
        .map_err(|e| CliError::unavailable(format!("cannot read Bonsai's /props: {e:#}")))?;
    describe::check_vision(&props).map_err(|e| CliError::fail(format!("{e:#}")))?;
    ctx.note(format!(
        "Bonsai looks at {} as {} (about {} s)...",
        file_label(&path),
        role.label().to_ascii_lowercase(),
        request::IMAGE_SECONDS
    ));
    let result = ctx.generate(session, &describe::messages(image, role), &describe::params(), run.stream)?;
    let described = describe::finish(&path, role, &result, &props.model_name)
        .map_err(|e| CliError::generation(format!("{e:#}")))?;
    if let Err(e) = cache.store(&described) {
        ctx.warn(format!("the description could not be cached: {e:#}"));
    }
    ctx.note(timing_line(&result));
    Ok(described)
}

/// The file name of a path (else the whole path).
fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// `describe --json` (and the facts of the MCP `describe_image` tool).
#[derive(Debug, Serialize)]
struct DescribeJson<'a> {
    path: &'a Path,
    role: &'static str,
    description: &'a str,
    /// From the cache: Bonsai was not asked.
    cached: bool,
    model: &'a str,
    created: &'a str,
    elapsed_ms: u64,
    timings: Option<&'a crate::client::Timings>,
    finish_reason: &'a str,
    started_server: bool,
    stopped_server: bool,
    out_file: Option<&'a Path>,
    copied: bool,
    warnings: &'a [String],
}

/// What `describe` (and MCP `describe_image`) produced.
struct DescribeRun {
    described: Described,
    started: bool,
    stopped: bool,
    delivery: Delivery,
    warnings: Vec<String>,
    session: Option<Session>,
}

impl DescribeRun {
    fn json(&self) -> DescribeJson<'_> {
        let d = &self.described;
        DescribeJson {
            path: &d.path,
            role: d.role.id(),
            description: &d.description,
            cached: d.cached,
            model: &d.model,
            created: &d.created,
            elapsed_ms: d.elapsed_ms,
            timings: d.timings.as_ref(),
            finish_reason: &d.finish_reason,
            started_server: self.started,
            stopped_server: self.stopped,
            out_file: self.delivery.out_file.as_deref(),
            copied: self.delivery.copied,
            warnings: &self.warnings,
        }
    }
}

/// Describe one picture end to end: cache or Bonsai, stop-after, `--out` / `--copy`.
fn run_describe(
    ctx: &Ctx,
    path: &Path,
    role: RefRole,
    use_cache: bool,
    run: &RunArgs,
) -> Result<DescribeRun, CliError> {
    let mut session = None;
    let described = describe_picture(ctx, path, role, use_cache, run, &mut session);
    let mut warnings = Vec::new();
    let stopped = match session.as_mut() {
        Some(s) => ctx.end_session(s, run.stop_after, &mut warnings),
        None => false,
    };
    let started = session.as_ref().is_some_and(|s| s.started);
    let described = described.inspect_err(|_| {
        if let Some(s) = &session {
            s.keep_running_note(ctx, run.stop_after);
        }
    })?;
    if described.finish_reason == "length" {
        let text = "Bonsai stopped at the token limit: its cut last sentence was left out".to_string();
        ctx.warn(&text);
        warnings.push(text);
    }
    let delivery = deliver(&described.description, run);
    warnings.extend(delivery.problems().cloned());
    Ok(DescribeRun { described, started, stopped, delivery, warnings, session })
}

fn cmd_describe(ctx: &Ctx, args: &DescribeArgs) -> CliResult {
    let done = run_describe(ctx, &args.image, args.role, !args.no_cache, &args.run)?;
    if ctx.json {
        print_json(&done.json())?;
    } else {
        print_line(&done.described.description);
    }
    done.delivery.report(ctx);
    if let Some(s) = &done.session {
        s.keep_running_note(ctx, args.run.stop_after);
    }
    Ok(if done.delivery.error.is_some() { EXIT_FAIL } else { EXIT_OK })
}

/// The job for a typed request, with the check facts read from it (`write`, MCP `write_prompt`).
fn text_job(mode: Mode, text: &str, params: GenParams) -> Result<WriteJob, CliError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CliError::usage("the request is empty: describe the pictures, the length and the action"));
    }
    let mut job = WriteJob::from_text(mode, text, params);
    job.lint_ctx = request::context_from_text(mode, &job.request);
    Ok(job)
}

fn cmd_fix(ctx: &Ctx, args: &FixArgs) -> CliResult {
    let (job, warnings) = fix_job(ctx, args)?;
    run_prompt_job(ctx, &job, &args.run, &args.prompt, warnings, None)
}

/// The follow-up job of `fix` (and MCP `fix_prompt`): the entry's check facts
/// ([`writer::entry_lint_context`]) with the options on top, and the fix text or the check's
/// problems. Also returns what to warn about (pictures of the entry that cannot be read).
fn fix_job(ctx: &Ctx, args: &FixArgs) -> Result<(WriteJob, Vec<String>), CliError> {
    let typed = args.fix.as_deref().map(str::trim);
    if typed == Some("") {
        return Err(CliError::usage("--fix is empty: say what to change (see `bh3 fixes`), or use --from-lint"));
    }
    let entry = find_entry(args.id.as_deref().unwrap_or("last"))?;
    let cfg = ctx.config();
    let params = args.gen.apply(cfg.writer_params.clone());
    let mut job = WriteJob::followup_of(&entry, typed.unwrap_or_default(), params);
    if entry.lint_context.is_none() && entry.spec.is_none() {
        // A text entry saved before the check facts were stored with it: read the missing facts
        // from its request, as `write` did.
        job.lint_ctx = request::fill_missing(
            job.lint_ctx.clone(),
            &request::context_from_text(entry.mode, entry.original_request()),
        );
    }
    job.lint_ctx = args.context.apply(job.lint_ctx.clone())?;
    let fix = match typed {
        Some(text) => text.to_string(),
        None => {
            let report = lint::lint(&entry.output, &job.lint_ctx);
            fix_from_report(&report).ok_or_else(|| {
                CliError::fail(format!(
                    "nothing to fix: the check finds no errors or warnings in {} ({})",
                    entry.id,
                    score_words(&report)
                ))
            })?
        }
    };
    job.request = fix.clone();
    if let Some(followup) = job.followup.as_mut() {
        followup.fix = fix;
    }
    job.save_history = cfg.save_history && !args.prompt.no_history;
    let mut warnings = Vec::new();
    // `followup_of` leaves all pictures out when one cannot be read: say so instead of silently
    // sending the fix without them.
    if let Some(spec) = entry.spec.as_ref().filter(|s| job.images.len() < s.sent_image_count()) {
        if let Err(e) = spec.images() {
            let text = format!("{e:#}: the fix is sent without the pictures of {}", entry.id);
            ctx.warn(&text);
            warnings.push(text);
        }
    }
    Ok((job, warnings))
}

/// A follow-up fix made of the check's errors and warnings (`None` when it found neither).
fn fix_from_report(report: &LintReport) -> Option<String> {
    let mut problems: Vec<String> = Vec::new();
    for issue in report.issues.iter().filter(|i| i.severity != Severity::Info) {
        let line = format!("- {}", issue.message.trim());
        if !problems.contains(&line) {
            problems.push(line);
        }
    }
    if problems.is_empty() {
        return None;
    }
    Some(format!("Fix these problems and keep everything else the same:\n{}", problems.join("\n")))
}

/// Run a write job end to end and print the result (`warnings`: problems found before the run).
/// `session`: a Bonsai already in use (and maybe started) by this command (`write --describe`).
fn run_prompt_job(
    ctx: &Ctx,
    job: &WriteJob,
    run: &RunArgs,
    prompt: &PromptArgs,
    warnings: Vec<String>,
    session: Option<Session>,
) -> CliResult {
    if prompt.dry_run {
        return print_dry_run(ctx, job, &writer::build_messages(&Library::load(), job));
    }
    let done = run_job(ctx, job, run, warnings, session)?;
    let outcome = &done.outcome;
    if ctx.json {
        print_json(&WriteJson {
            mode: outcome.lint.mode,
            prompt: &outcome.prompt,
            reasoning: &outcome.reasoning,
            lint: LintJson::new(&outcome.lint),
            lint_context: &done.lint_context,
            result: &outcome.result,
            history_id: outcome.history_id.as_deref(),
            parent_id: job.followup.as_ref().and_then(|f| f.parent_id.as_deref()),
            started_server: done.session.started,
            stopped_server: done.stopped,
            out_file: done.delivery.out_file.as_deref(),
            copied: done.delivery.copied,
            warnings: &done.warnings,
        })?;
    } else {
        print_line(&outcome.prompt);
    }
    report_write(ctx, outcome, &done.lint_context, &done.delivery, &done.late);
    done.session.keep_running_note(ctx, run.stop_after);
    let failed = done.delivery.error.is_some() || (prompt.strict && !outcome.lint.ok);
    Ok(if failed { EXIT_FAIL } else { EXIT_OK })
}

/// What a finished write / fix job produced.
struct JobRun {
    outcome: WriteOutcome,
    /// The facts the check used.
    lint_context: LintContext,
    session: Session,
    /// Bonsai was stopped afterwards (`--stop-after`).
    stopped: bool,
    delivery: Delivery,
    /// Problems reported after the check summary (token limit, history not saved).
    late: Vec<String>,
    /// Every problem that did not stop the job (JSON `warnings`).
    warnings: Vec<String>,
}

/// Run a write job: server, request, cleanup + check + history, stop-after, `--out` / `--copy`.
/// Progress goes to stderr only (the MCP server uses it too); `warnings` start the warning list.
/// `session`: a Bonsai this command already uses (else it is made ready here).
fn run_job(
    ctx: &Ctx,
    job: &WriteJob,
    run: &RunArgs,
    mut warnings: Vec<String>,
    session: Option<Session>,
) -> Result<JobRun, CliError> {
    let messages = writer::build_messages(&Library::load(), job);
    let mut session = match session {
        Some(session) => session,
        None => ctx.ensure_ready(run)?,
    };
    ctx.note(job_line(job));
    if job.params.thinking != ThinkingMode::Off {
        ctx.note(format!(
            "note: thinking {}: {} Off is tested best for prompts.",
            job.params.thinking,
            job.params.thinking.hint()
        ));
    }
    if !job.images.is_empty() {
        ctx.note(format!(
            "note: {} go to Bonsai's vision input: about {} tokens and {} s more.",
            plural(job.images.len(), "picture"),
            job.images.len() * request::IMAGE_TOKENS,
            job.images.len() * request::IMAGE_SECONDS
        ));
    }
    let generated = ctx.generate(&mut session, &messages, &job.params, run.stream);
    let finished = generated.and_then(|result| {
        // Checked before `finish` so an empty answer is not saved to the history.
        if writer::clean_output(&result.content).trim().is_empty() {
            Err(CliError::generation(empty_answer_message(&result.finish_reason, &job.params)))
        } else {
            Ok(writer::finish(job, result))
        }
    });
    // A failed stop is printed by `end_session` already and added to the warnings.
    let stopped = ctx.end_session(&mut session, run.stop_after, &mut warnings);
    let outcome = finished.inspect_err(|_| session.keep_running_note(ctx, run.stop_after))?;

    let mut late = Vec::new();
    if outcome.result.finish_reason == "length" {
        late.push(format!(
            "Bonsai stopped at the token limit ({} tokens): the prompt may be cut off - raise --max-tokens",
            job.params.max_tokens
        ));
    }
    if job.save_history && outcome.history_id.is_none() {
        late.push(format!("the prompt could not be saved to the history ({})", history::history_dir().display()));
    }
    let lint_context = writer::lint_context_for(job, &outcome.prompt);
    let delivery = deliver(&outcome.prompt, run);
    warnings.extend(late.iter().cloned());
    warnings.extend(delivery.problems().cloned());
    Ok(JobRun { outcome, lint_context, session, stopped, delivery, late, warnings })
}

/// `write --dry-run --json` / `fix --dry-run --json`.
#[derive(Debug, Serialize)]
struct DryRunJson<'a> {
    mode: Mode,
    /// The request (`write`) or the fix (`fix`) Bonsai would get.
    request: &'a str,
    /// `fix`: the history entry the fix follows up on.
    parent_id: Option<&'a str>,
    lint_context: &'a LintContext,
    params: &'a GenParams,
    /// Pictures for the vision input.
    images: usize,
    /// Chat messages: system prompt and request (a fix adds the previous answer and the fix).
    messages: usize,
    save_history: bool,
}

/// `--dry-run`: the request (or fix) on stdout, nothing sent.
fn print_dry_run(ctx: &Ctx, job: &WriteJob, messages: &[ChatMessage]) -> CliResult {
    let request = job.followup.as_ref().map_or(job.request.as_str(), |f| f.fix.as_str());
    if ctx.json {
        print_json(&DryRunJson {
            mode: job.mode,
            request,
            parent_id: job.followup.as_ref().and_then(|f| f.parent_id.as_deref()),
            lint_context: &job.lint_ctx,
            params: &job.params,
            images: job.images.len(),
            messages: messages.len(),
            save_history: job.save_history,
        })?;
    } else {
        print_line(request);
        ctx.note(format!("Dry run, nothing was sent to Bonsai. {}", job_line(job)));
    }
    Ok(EXIT_OK)
}

/// What `--out` / `--copy` did (reported after the result).
#[derive(Debug, Default)]
struct Delivery {
    out_file: Option<PathBuf>,
    copied: bool,
    /// `Saved to ...`, `Copied ...`.
    notes: Vec<String>,
    /// The clipboard failed.
    warning: Option<String>,
    /// `--out` failed (exit 1).
    error: Option<String>,
}

impl Delivery {
    fn problems(&self) -> impl Iterator<Item = &String> {
        self.warning.iter().chain(self.error.iter())
    }

    fn report(&self, ctx: &Ctx) {
        for note in &self.notes {
            ctx.note(note);
        }
        if let Some(warning) = &self.warning {
            ctx.warn(warning);
        }
        if let Some(error) = &self.error {
            eprint_line(&format!("error: {error}"));
        }
    }
}

/// `--out FILE` and `--copy` for a finished text.
fn deliver(text: &str, run: &RunArgs) -> Delivery {
    let mut delivery = Delivery::default();
    if let Some(path) = &run.out {
        match history::write_notepad_txt(text, path) {
            Ok(()) => {
                delivery.notes.push(format!("Saved to {}.", path.display()));
                delivery.out_file = Some(path.clone());
            }
            Err(e) => delivery.error = Some(format!("could not save {}: {e:#}", path.display())),
        }
    }
    if run.copy {
        match copy_to_clipboard(text) {
            Ok(()) => {
                delivery.notes.push("Copied to the clipboard.".to_string());
                delivery.copied = true;
            }
            Err(message) => delivery.warning = Some(message),
        }
    }
    delivery
}

/// The stderr summary after a written prompt: speed, check result, files, warnings, history id.
fn report_write(ctx: &Ctx, outcome: &WriteOutcome, lint_ctx: &LintContext, delivery: &Delivery, late: &[String]) {
    ctx.note(timing_line(&outcome.result));
    if !ctx.quiet {
        for line in lint_summary_lines(&outcome.lint, lint_ctx) {
            eprint_line(&line);
        }
    }
    delivery.report(ctx);
    for warning in late {
        ctx.warn(warning);
    }
    if let Some(id) = &outcome.history_id {
        if outcome.lint.errors() + outcome.lint.warnings() > 0 {
            ctx.note(format!(
                "Saved to the history as {id}. To fix what the check found: bh3 fix --id {id} --from-lint"
            ));
        } else {
            ctx.note(format!("Saved to the history as {id}."));
        }
    }
}

/// `Writing an r2v prompt (6 s = 6.58 s real, 2 pictures; thinking off, temperature 0.4)...`, or
/// for a follow-up `Fixing <id> (r2v, ...): <fix>`.
fn job_line(job: &WriteJob) -> String {
    let ctx = &job.lint_ctx;
    let mut facts: Vec<String> = Vec::new();
    if job.followup.is_some() {
        facts.push(job.mode.to_string());
    }
    if let Some(d) = ctx.duration_s {
        facts.push(format!("{} s = {} s real", fmt_num(d), lengths::format_real(d)));
    }
    if let Some(n) = ctx.ref_count {
        facts.push(plural(n, "picture"));
    }
    if ctx.first_frame_pinned == Some(true) {
        facts.push("Ref 1 is the first frame".to_string());
    }
    if ctx.has_last_frame == Some(true) {
        facts.push("with last frame".to_string());
    }
    let mut details = facts.join(", ");
    if !details.is_empty() {
        details.push_str("; ");
    }
    details.push_str(&format!("thinking {}, temperature {}", job.params.thinking, fmt_num(job.params.temperature)));
    match &job.followup {
        Some(followup) => {
            let parent = followup.parent_id.as_deref().unwrap_or("the previous prompt");
            format!("Fixing {parent} ({details}): {}", followup.fix.trim())
        }
        None => format!("Writing an {} prompt ({details})...", job.mode),
    }
}

/// Error text for an answer that is empty after cleanup.
fn empty_answer_message(finish_reason: &str, params: &GenParams) -> String {
    let reason = match finish_reason.trim() {
        "" => "none",
        other => other,
    };
    let mut message = format!("Bonsai returned an empty answer (finish_reason {reason})");
    if reason == "length" {
        if params.thinking != ThinkingMode::Off {
            message.push_str(&format!(
                " - thinking used all {} tokens: use --thinking off or raise --max-tokens",
                params.max_tokens
            ));
        } else {
            message.push_str(&format!(" - raise --max-tokens (now {})", params.max_tokens));
        }
    }
    message
}

/// `Done in 14.2 s: 120 new + 1800 cached prompt tokens, 640 generated at 42.0 tok/s.`
fn timing_line(result: &ChatResult) -> String {
    let secs = result.elapsed_ms as f64 / 1000.0;
    match &result.timings {
        Some(t) => format!(
            "Done in {secs:.1} s: {} new + {} cached prompt tokens, {} generated at {:.1} tok/s.",
            t.prompt_n, t.cache_n, t.predicted_n, t.predicted_per_second
        ),
        None => format!("Done in {secs:.1} s."),
    }
}

/// `Check: score 92 - 0 errors, 1 warning, 2 info (r2v, 6 s, 2 pictures)` + one line per error / warning.
fn lint_summary_lines(report: &LintReport, ctx: &LintContext) -> Vec<String> {
    let infos = report.issues.iter().filter(|i| i.severity == Severity::Info).count();
    let mut lines = vec![format!("Check: {} ({})", score_words(report), context_words(ctx))];
    for issue in report.issues.iter().filter(|i| i.severity != Severity::Info) {
        lines.push(format!("  {}", issue_line(issue)));
    }
    if infos > 0 {
        lines.push(format!("  ({infos} info not shown; --json lists every issue)"));
    }
    lines
}

/// `score 92 - 0 errors, 1 warning, 2 info`.
fn score_words(report: &LintReport) -> String {
    let infos = report.issues.iter().filter(|i| i.severity == Severity::Info).count();
    format!(
        "score {} - {}, {}, {infos} info",
        report.score,
        plural(report.errors(), "error"),
        plural(report.warnings(), "warning")
    )
}

/// `r2v, 6 s, 2 pictures, first frame pinned`.
fn context_words(ctx: &LintContext) -> String {
    let mut words = vec![ctx.mode.to_string()];
    if let Some(d) = ctx.duration_s {
        words.push(format!("{} s", fmt_num(d)));
    }
    if let Some(n) = ctx.ref_count {
        words.push(plural(n, "picture"));
    }
    match (ctx.mode, ctx.first_frame_pinned, ctx.has_last_frame) {
        (Mode::R2v, Some(true), _) => words.push("first frame pinned".to_string()),
        (Mode::F2v, _, Some(true)) => words.push("with last frame".to_string()),
        (Mode::F2v, _, Some(false)) => words.push("first frame only".to_string()),
        _ => {}
    }
    words.join(", ")
}

/// `error shot-time-format (line 19): ...` (the line is left out when the message names it).
fn issue_line(issue: &LintIssue) -> String {
    let line = match issue.line {
        Some(l) if !issue.message.contains(&format!("(line {l})")) => format!(" (line {l})"),
        _ => String::new(),
    };
    format!("{} {}{line}: {}", severity_name(issue.severity), issue.code, issue.message)
}

/// The serde names, so text and JSON output agree.
fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

// ---------------------------------------------------------------------------------------------
// Check facts (lint context)
// ---------------------------------------------------------------------------------------------

impl ContextArgs {
    /// The options on top of `ctx`. In f2v, `--refs 2` implies a last frame and `--last-frame`
    /// two pictures (unless both are given).
    fn apply(&self, mut ctx: LintContext) -> Result<LintContext, CliError> {
        if let Some(seconds) = self.duration {
            ctx.duration_s = Some(seconds);
        }
        if let Some(n) = self.refs {
            let max = request::max_refs(ctx.mode);
            if n > max {
                return Err(CliError::usage(format!("--refs {n}: {} prompts use at most {max} pictures", ctx.mode)));
            }
            ctx.ref_count = Some(n);
            if ctx.mode == Mode::F2v && self.last_frame.is_none() {
                ctx.has_last_frame = Some(n >= 2);
            }
        }
        if let Some(pinned) = self.pinned {
            ctx.first_frame_pinned = Some(pinned);
        }
        if let Some(last) = self.last_frame {
            ctx.has_last_frame = Some(last);
            if ctx.mode == Mode::F2v && self.refs.is_none() {
                ctx.ref_count = Some(if last { 2 } else { 1 });
            }
        }
        Ok(ctx)
    }
}

// ---------------------------------------------------------------------------------------------
// lint
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct LintCmdJson<'a> {
    #[serde(flatten)]
    lint: LintJson<'a>,
    context: &'a LintContext,
}

fn cmd_lint(ctx: &Ctx, args: &LintArgs) -> CliResult {
    let text = match &args.file {
        Some(path) => read_text_file(path)?,
        None => read_stdin_text("the prompt to check")?,
    };
    let mode = match args.mode {
        Some(mode) => mode.into(),
        None => lint::detect_mode(&text).unwrap_or_else(|| {
            ctx.note("note: the text is neither an r2v nor an f2v prompt; checking it as r2v (use --mode)");
            Mode::R2v
        }),
    };
    let lint_ctx = args.context.apply(LintContext { mode, ..LintContext::default() })?;
    let report = lint::lint(&text, &lint_ctx);
    if ctx.json {
        print_json(&LintCmdJson { lint: LintJson::new(&report), context: &lint_ctx })?;
    } else {
        print_line(&lint_text(&report, &lint_ctx));
    }
    Ok(if report.ok { EXIT_OK } else { EXIT_FAIL })
}

fn lint_text(report: &LintReport, ctx: &LintContext) -> String {
    let mut lines = vec![format!("{} prompt: {} ({})", report.mode, score_words(report), context_words(ctx))];
    let shots: Vec<String> = report
        .shots
        .iter()
        .map(|s| match s.time_s {
            Some(t) => format!("[Shot {}] at {t:.3} s", s.number),
            None => format!("[Shot {}]", s.number),
        })
        .collect();
    lines.push(format!(
        "{}, description {}, {}{}",
        plural(report.word_count, "word"),
        plural(report.description_words, "word"),
        plural(report.shots.len(), "shot"),
        if shots.is_empty() { String::new() } else { format!(": {}", shots.join(", ")) }
    ));
    for issue in &report.issues {
        lines.push(format!("  {}", issue_line(issue)));
    }
    lines.push(if report.ok {
        "OK: no errors.".to_string()
    } else {
        "Not ready: fix the errors before rendering.".to_string()
    });
    lines.join("\n")
}

// ---------------------------------------------------------------------------------------------
// chat
// ---------------------------------------------------------------------------------------------

/// `chat --json`: the serde form of [`ChatResult`] plus the run facts.
#[derive(Debug, Serialize)]
struct ChatJson<'a> {
    #[serde(flatten)]
    result: &'a ChatResult,
    started_server: bool,
    stopped_server: bool,
    out_file: Option<&'a Path>,
    copied: bool,
    warnings: &'a [String],
}

fn cmd_chat(ctx: &Ctx, args: &ChatArgs) -> CliResult {
    let message = match (&args.message, &args.message_file) {
        (Some(text), _) => normalize_text(text),
        (None, Some(path)) => read_text_file(path)?,
        (None, None) => read_stdin_text("the message")?,
    };
    if message.trim().is_empty() {
        return Err(CliError::usage("the message is empty"));
    }
    let mut messages = Vec::new();
    match &args.system {
        SystemArg::None => {}
        SystemArg::Mode(mode) => messages.push(ChatMessage::system(Library::load().system_prompt(*mode))),
        SystemArg::File(path) => {
            let text = read_text_file(path)?;
            if text.trim().is_empty() {
                return Err(CliError::usage(format!("the system prompt file {} is empty", path.display())));
            }
            messages.push(ChatMessage::system(text));
        }
    }
    let mut images = Vec::new();
    for path in &args.images {
        images.push(ImageAttachment::from_file(path).map_err(CliError::from)?);
    }
    messages.push(ChatMessage::user(message.trim()).with_images(images));
    let params = args.gen.apply(ctx.config().chat_params.clone());

    let done = run_chat(ctx, &messages, &params, &args.run)?;
    if ctx.json {
        print_json(&ChatJson {
            result: &done.result,
            started_server: done.session.started,
            stopped_server: done.stopped,
            out_file: done.delivery.out_file.as_deref(),
            copied: done.delivery.copied,
            warnings: &done.warnings,
        })?;
    } else {
        print_line(done.result.content.trim());
    }
    ctx.note(timing_line(&done.result));
    done.delivery.report(ctx);
    for warning in &done.late {
        ctx.warn(warning);
    }
    done.session.keep_running_note(ctx, args.run.stop_after);
    Ok(if done.delivery.error.is_some() { EXIT_FAIL } else { EXIT_OK })
}

/// What a finished chat request produced.
struct ChatRun {
    result: ChatResult,
    session: Session,
    /// Bonsai was stopped afterwards (`--stop-after`).
    stopped: bool,
    delivery: Delivery,
    /// Problems reported after the answer (token limit).
    late: Vec<String>,
    /// Every problem that did not stop the request (JSON `warnings`).
    warnings: Vec<String>,
}

/// Send one chat request (`chat`, MCP `chat`): server, request, stop-after, `--out` / `--copy`.
/// Progress goes to stderr only; an empty answer is an error (exit 4).
fn run_chat(ctx: &Ctx, messages: &[ChatMessage], params: &GenParams, run: &RunArgs) -> Result<ChatRun, CliError> {
    let mut session = ctx.ensure_ready(run)?;
    let generated = ctx.generate(&mut session, messages, params, run.stream);
    let mut warnings = Vec::new();
    let stopped = ctx.end_session(&mut session, run.stop_after, &mut warnings);
    let result = generated
        .and_then(|result| {
            if result.content.trim().is_empty() {
                Err(CliError::generation(empty_answer_message(&result.finish_reason, params)))
            } else {
                Ok(result)
            }
        })
        .inspect_err(|_| session.keep_running_note(ctx, run.stop_after))?;
    let mut late = Vec::new();
    if result.finish_reason == "length" {
        late.push(format!("Bonsai stopped at the token limit ({} tokens) - raise --max-tokens", params.max_tokens));
    }
    let delivery = deliver(result.content.trim(), run);
    warnings.extend(late.iter().cloned());
    warnings.extend(delivery.problems().cloned());
    Ok(ChatRun { result, session, stopped, delivery, late, warnings })
}

// ---------------------------------------------------------------------------------------------
// Library: templates, fixes, system prompts, guides
// ---------------------------------------------------------------------------------------------

/// A template with its 1-based number in the full list.
#[derive(Debug, Serialize)]
struct TemplateJson<'a> {
    number: usize,
    #[serde(flatten)]
    template: &'a Template,
}

fn cmd_templates(ctx: &Ctx, args: &TemplatesArgs) -> CliResult {
    let templates = Library::load().templates();
    let wanted: Option<Mode> = args.mode.map(Into::into);
    let shown: Vec<TemplateJson<'_>> = templates
        .iter()
        .enumerate()
        .filter(|(_, t)| wanted.is_none_or(|mode| t.mode == mode))
        .map(|(i, template)| TemplateJson { number: i + 1, template })
        .collect();
    if ctx.json {
        print_json(&shown)?;
        return Ok(EXIT_OK);
    }
    let width = shown.iter().map(|t| t.template.id.chars().count()).max().unwrap_or(2).max(2);
    let mut lines = vec![format!("{:>2}  {:<width$}  MODE  TITLE", "#", "ID")];
    for t in &shown {
        lines.push(format!(
            "{:>2}  {:<width$}  {:<4}  {}",
            t.number,
            t.template.id,
            t.template.mode.as_str(),
            t.template.title
        ));
    }
    print_line(&lines.join("\n"));
    ctx.note("Print one with `bh3 template <ID>`, fill its <...> parts and pass it to `bh3 write`.");
    Ok(EXIT_OK)
}

fn cmd_template(ctx: &Ctx, args: &TemplateArgs) -> CliResult {
    let templates = Library::load().templates();
    let key = args.id.trim();
    let found = match key.parse::<usize>() {
        Ok(n) if n >= 1 => templates.get(n - 1),
        _ => templates.iter().find(|t| t.id.eq_ignore_ascii_case(key)),
    };
    let Some(template) = found else {
        let ids: Vec<&str> = templates.iter().map(|t| t.id.as_str()).collect();
        return Err(CliError::fail(format!("no template '{key}'. Templates: {}", ids.join(", "))));
    };
    if ctx.json {
        print_json(template)?;
    } else {
        print_line(&template.text);
        ctx.note(format!("({} template \"{}\": fill the <...> parts.)", template.mode, template.title));
    }
    Ok(EXIT_OK)
}

fn cmd_fixes(ctx: &Ctx, args: &FixesArgs) -> CliResult {
    let library = Library::load();
    let fixes = match args.mode {
        Some(mode) => library.fixes_for(mode.into()),
        None => library.fixes(),
    };
    if ctx.json {
        print_json(&fixes)?;
        return Ok(EXIT_OK);
    }
    let lines: Vec<String> = fixes.iter().enumerate().map(|(i, fix)| format!("{:>2}. {fix}", i + 1)).collect();
    print_line(&lines.join("\n"));
    ctx.note("Send one with `bh3 fix --last --fix \"<text>\"` (fill its <...> parts first).");
    Ok(EXIT_OK)
}

#[derive(Debug, Serialize)]
struct LibraryTextJson<'a> {
    mode: Mode,
    item: &'static str,
    /// The text is the user's edited version (Library workspace), not the built-in one.
    overridden: bool,
    /// The official MiniMax guide (guides only).
    official_guide: Option<&'static str>,
    text: &'a str,
}

/// `system-prompt <mode>` and `guide <mode>`.
fn cmd_library_text(ctx: &Ctx, mode: Mode, guide: bool) -> CliResult {
    let library = Library::load();
    let item = match (guide, mode) {
        (false, Mode::R2v) => LibraryItem::SystemR2v,
        (false, Mode::F2v) => LibraryItem::SystemF2v,
        (true, Mode::R2v) => LibraryItem::GuideR2v,
        (true, Mode::F2v) => LibraryItem::GuideF2v,
    };
    let text = library.text(item);
    if ctx.json {
        let official_guide = match (guide, mode) {
            (false, _) => None,
            (true, Mode::R2v) => Some(crate::GUIDE_REF_URL),
            (true, Mode::F2v) => Some(crate::GUIDE_BASE_URL),
        };
        print_json(&LibraryTextJson {
            mode,
            item: item.id(),
            overridden: library.is_overridden(item),
            official_guide,
            text,
        })?;
    } else {
        print_line(text);
    }
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------------------------

/// One row of `history list --json`.
#[derive(Debug, Serialize)]
struct HistorySummary<'a> {
    id: &'a str,
    created: &'a str,
    date: String,
    mode: Mode,
    title: &'a str,
    lint_score: Option<u8>,
    lint_errors: usize,
    duration_s: Option<u32>,
    starred: bool,
    parent_id: Option<&'a str>,
    /// The fix of a follow-up entry.
    fix: Option<&'a str>,
    words: usize,
    tokens_per_second: Option<f64>,
}

impl<'a> HistorySummary<'a> {
    fn of(e: &'a HistoryEntry) -> Self {
        Self {
            id: &e.id,
            created: &e.created,
            date: e.short_date(),
            mode: e.mode,
            title: &e.title,
            lint_score: e.lint_score,
            lint_errors: e.lint_errors,
            duration_s: e.duration_s,
            starred: e.starred,
            parent_id: e.parent_id.as_deref(),
            fix: e.fix_text(),
            words: e.output.split_whitespace().count(),
            tokens_per_second: e.timings.as_ref().map(|t| (t.predicted_per_second * 10.0).round() / 10.0),
        }
    }
}

fn cmd_history(ctx: &Ctx, cmd: &HistoryCmd) -> CliResult {
    match cmd {
        HistoryCmd::List(args) => history_list(ctx, args),
        HistoryCmd::Show(args) => history_show(ctx, args),
        HistoryCmd::Delete(args) => {
            let id = args.id.trim();
            if id.eq_ignore_ascii_case("last") || id.eq_ignore_ascii_case("latest") {
                return Err(CliError::usage("delete needs the entry's id (see `bh3 history list`), not `last`"));
            }
            history::delete(id).map_err(|e| CliError::fail(format!("{e:#}")))?;
            if ctx.json {
                print_json(&json!({ "deleted": id }))?;
            } else {
                print_line(&format!("Deleted {id}."));
            }
            Ok(EXIT_OK)
        }
        HistoryCmd::Export(args) => {
            let entry = find_entry(&args.id)?;
            history::export_txt(&entry, &args.file).map_err(CliError::from)?;
            if ctx.json {
                print_json(&json!({ "id": entry.id, "file": args.file }))?;
            } else {
                print_line(&format!("Saved the prompt of {} to {}.", entry.id, args.file.display()));
            }
            Ok(EXIT_OK)
        }
    }
}

fn history_list(ctx: &Ctx, args: &HistoryListArgs) -> CliResult {
    let wanted: Option<Mode> = args.mode.map(Into::into);
    let query = args.search.as_deref().unwrap_or("");
    let mut entries: Vec<HistoryEntry> = history::list()
        .into_iter()
        .filter(|e| wanted.is_none_or(|mode| e.mode == mode))
        .filter(|e| !args.starred || e.starred)
        .filter(|e| e.matches(query))
        .collect();
    let total = entries.len();
    if args.limit > 0 {
        entries.truncate(args.limit);
    }
    if ctx.json {
        print_json(&entries.iter().map(HistorySummary::of).collect::<Vec<_>>())?;
        return Ok(EXIT_OK);
    }
    if entries.is_empty() {
        ctx.note(format!("No saved prompts (history folder: {}).", history::history_dir().display()));
        return Ok(EXIT_OK);
    }
    let width = entries.iter().map(|e| e.id.chars().count()).max().unwrap_or(2).max(2);
    let mut lines = vec![format!("{:<width$}  {:<16}  MODE  SCORE  ERR  TITLE", "ID", "DATE")];
    for e in &entries {
        let score = e.lint_score.map_or_else(|| "-".to_string(), |s| s.to_string());
        let star = if e.starred { "* " } else { "" };
        lines.push(format!(
            "{:<width$}  {:<16}  {:<4}  {:>5}  {:>3}  {star}{}",
            e.id,
            e.short_date(),
            e.mode.as_str(),
            score,
            e.lint_errors,
            e.title
        ));
    }
    print_line(&lines.join("\n"));
    if total > entries.len() {
        ctx.note(format!("{} of {total} shown (--limit 0 shows all).", entries.len()));
    }
    Ok(EXIT_OK)
}

fn history_show(ctx: &Ctx, args: &HistoryShowArgs) -> CliResult {
    let entry = find_entry(&args.id)?;
    let field = match args.field {
        Some(FieldArg::Json) => None,
        other => other,
    };
    if args.field == Some(FieldArg::Json) || (ctx.json && field.is_none()) {
        print_json(&entry)?;
        return Ok(EXIT_OK);
    }
    let Some(field) = field else {
        print_line(&entry_text(&entry));
        return Ok(EXIT_OK);
    };
    let value = match field {
        FieldArg::Prompt => entry.output.as_str(),
        FieldArg::Request => entry.original_request(),
        FieldArg::Fix => entry.fix_text().unwrap_or(""),
        FieldArg::Reasoning => entry.reasoning.as_str(),
        FieldArg::Json => "",
    };
    if ctx.json {
        let mut map = serde_json::Map::new();
        map.insert("id".to_string(), json!(entry.id));
        map.insert(field.name().to_string(), json!(value));
        print_json(&map)?;
    } else {
        print_line(value);
    }
    Ok(EXIT_OK)
}

/// A history entry by id, or the newest for `last`.
fn find_entry(id: &str) -> Result<HistoryEntry, CliError> {
    let id = id.trim();
    if id.eq_ignore_ascii_case("last") || id.eq_ignore_ascii_case("latest") {
        return history::latest()
            .ok_or_else(|| CliError::fail("no saved prompts yet (bh3 write saves every prompt it writes)"));
    }
    history::load(id).map_err(|e| CliError::fail(format!("{e:#} (see `bh3 history list`)")))
}

/// The readable `history show` view.
fn entry_text(e: &HistoryEntry) -> String {
    let mut lines = vec![
        format!("{:<10}{}", "id", e.id),
        format!("{:<10}{}", "date", e.short_date()),
        format!("{:<10}{}", "mode", e.mode),
        format!("{:<10}{}", "title", e.title),
    ];
    if let Some(score) = e.lint_score {
        lines.push(format!("{:<10}score {score}, {}", "check", plural(e.lint_errors, "error")));
    }
    if let Some(d) = e.duration_s {
        lines.push(format!("{:<10}{d} s ({} s real)", "length", lengths::format_real(d as f32)));
    }
    if let Some(parent) = &e.parent_id {
        lines.push(format!("{:<10}{parent}", "fix of"));
    }
    if e.starred {
        lines.push(format!("{:<10}yes", "starred"));
    }
    lines.push(format!(
        "{:<10}thinking {}, temperature {}, max tokens {}",
        "settings",
        e.params.thinking,
        fmt_num(e.params.temperature),
        e.params.max_tokens
    ));
    if let Some(t) = &e.timings {
        lines.push(format!("{:<10}{} tokens at {:.1} tok/s", "speed", t.predicted_n, t.predicted_per_second));
    }
    let mut section = |title: &str, text: &str| {
        lines.push(String::new());
        lines.push(format!("--- {title} ---"));
        lines.push(text.trim_end().to_string());
    };
    section("request", e.original_request());
    if let Some(fix) = e.fix_text() {
        section("fix", fix);
    }
    if !e.reasoning.trim().is_empty() {
        section("reasoning", &e.reasoning);
    }
    section("prompt", &e.output);
    lines.join("\n")
}

// ---------------------------------------------------------------------------------------------
// lengths / tokens
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct LengthRow {
    /// Requested seconds.
    seconds: f64,
    /// Frames H3 renders (24 fps, 17k+5 grid).
    frames: u32,
    /// Real length with two decimals, as the prompts use it.
    real: String,
    real_s: f64,
    /// Most cuts that still leave each shot time to play out.
    max_cuts: u32,
    /// Largest output size that rendered reliably on the 8 GB laptop.
    max_megapixels: f64,
}

impl LengthRow {
    fn of(seconds: f32) -> Self {
        let real = lengths::format_real(seconds);
        Self {
            seconds: round3(seconds),
            frames: lengths::frames_for_seconds(seconds),
            real_s: real.parse().unwrap_or(0.0),
            real,
            max_cuts: lengths::suggested_max_cuts(seconds),
            max_megapixels: round3(lengths::laptop_max_megapixels(seconds)),
        }
    }
}

fn cmd_lengths(ctx: &Ctx, args: &LengthsArgs) -> CliResult {
    let seconds: Vec<f32> = if args.seconds.is_empty() {
        lengths::DURATIONS.iter().map(|&s| s as f32).collect()
    } else {
        args.seconds.clone()
    };
    let rows: Vec<LengthRow> = seconds.into_iter().map(LengthRow::of).collect();
    if ctx.json {
        print_json(&rows)?;
        return Ok(EXIT_OK);
    }
    let mut lines = vec!["Requested  Frames  Real length  Max cuts  Max size (8 GB laptop)".to_string()];
    for row in &rows {
        lines.push(format!(
            "{:>9}  {:>6}  {:>11}  {:>8}  {} MP",
            format!("{} s", fmt_num(row.seconds as f32)),
            row.frames,
            format!("{} s", row.real),
            row.max_cuts,
            row.max_megapixels
        ));
    }
    print_line(&lines.join("\n"));
    Ok(EXIT_OK)
}

fn cmd_tokens(ctx: &Ctx, args: &TokensArgs) -> CliResult {
    let text = match (&args.text, &args.file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => read_text_file(path)?,
        (None, None) => read_stdin_text("the text to count")?,
    };
    let url = ctx.url();
    let mut mgr = ctx.manager(None);
    if !mgr.refresh().is_ready() {
        return Err(CliError::unavailable(format!(
            "Bonsai is not running at {url}: counting tokens needs it (start it with `bh3 start --wait`)"
        )));
    }
    let count = mgr.client().tokenize_count(&text).map_err(CliError::from)?;
    if ctx.json {
        print_json(&json!({ "tokens": count, "chars": text.chars().count() }))?;
    } else {
        print_line(&count.to_string());
    }
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------------------------
// config / install
// ---------------------------------------------------------------------------------------------

/// Settings `config get` / `config set` know, in display order.
const CONFIG_KEYS: [&str; 36] = [
    "bonsai_dir",
    "runtime.model",
    "runtime.mmproj",
    "runtime.server_exe",
    "runtime.dir",
    "runtime.models_dir",
    "runtime.hf_token",
    "server_url",
    "auto_start",
    "auto_stop_idle_minutes",
    "stop_on_exit",
    "vram_busy_mb",
    "ctx_size",
    "mmproj_cpu",
    "gpu_layers",
    "image_max_tokens",
    "kv_cache_4bit",
    "extra_args",
    "start_timeout_secs",
    "default_duration",
    "save_history",
    "auto_describe",
    "writer.thinking",
    "writer.temperature",
    "writer.max_tokens",
    "writer.top_p",
    "writer.top_k",
    "writer.min_p",
    "writer.seed",
    "chat.thinking",
    "chat.temperature",
    "chat.max_tokens",
    "chat.top_p",
    "chat.top_k",
    "chat.min_p",
    "chat.seed",
];

/// A setting's value.
#[derive(Debug, Clone, PartialEq)]
enum ConfigValue {
    Text(String),
    Path(Option<PathBuf>),
    Bool(bool),
    Count(u32),
    Int(i32),
    Real(f32),
    Seed(Option<i64>),
    Thinking(ThinkingMode),
    /// A number or `auto` (unset).
    Auto(Option<u32>),
    /// Free text (may be empty or contain spaces).
    Args(String),
    /// Shown as `***` when set.
    Secret(Option<String>),
}

impl ConfigValue {
    /// Plain-text form (`none` for an empty path or seed).
    fn text(&self) -> String {
        match self {
            ConfigValue::Text(s) => s.clone(),
            ConfigValue::Path(p) => p.as_ref().map_or_else(|| "none".to_string(), |p| p.display().to_string()),
            ConfigValue::Bool(b) => b.to_string(),
            ConfigValue::Count(n) => n.to_string(),
            ConfigValue::Int(n) => n.to_string(),
            ConfigValue::Real(x) => fmt_num(*x),
            ConfigValue::Seed(s) => s.map_or_else(|| "none".to_string(), |s| s.to_string()),
            ConfigValue::Thinking(t) => t.to_string(),
            ConfigValue::Auto(n) => n.map_or_else(|| "auto".to_string(), |n| n.to_string()),
            ConfigValue::Args(s) => s.clone(),
            ConfigValue::Secret(s) => s.as_ref().map_or_else(|| "none".to_string(), |_| "***".to_string()),
        }
    }

    fn json(&self) -> serde_json::Value {
        match self {
            ConfigValue::Text(s) => json!(s),
            ConfigValue::Path(p) => json!(p),
            ConfigValue::Bool(b) => json!(b),
            ConfigValue::Count(n) => json!(n),
            ConfigValue::Int(n) => json!(n),
            ConfigValue::Real(x) => json!(round3(*x)),
            ConfigValue::Seed(s) => json!(s),
            ConfigValue::Thinking(t) => json!(t),
            ConfigValue::Auto(n) => json!(n),
            ConfigValue::Args(s) => json!(s),
            ConfigValue::Secret(s) => json!(s.as_ref().map(|_| "***")),
        }
    }
}

/// `Writer_Params.Top-P` -> `writer.top_p`.
fn normalize_key(key: &str) -> String {
    let key: String = key.trim().to_ascii_lowercase().chars().map(|c| if c == '-' { '_' } else { c }).collect();
    for (long, short) in [("writer_params.", "writer."), ("chat_params.", "chat.")] {
        if let Some(rest) = key.strip_prefix(long) {
            return format!("{short}{rest}");
        }
    }
    key
}

fn unknown_key(key: &str) -> CliError {
    CliError::usage(format!("unknown setting '{key}'. Settings: {}", CONFIG_KEYS.join(", ")))
}

fn config_get(cfg: &AppConfig, key: &str) -> Result<(String, ConfigValue), CliError> {
    let key = normalize_key(key);
    if let Some(field) = key.strip_prefix("runtime.") {
        let rt = &cfg.runtime;
        let value = match field {
            "model" => ConfigValue::Path(rt.model.clone()),
            "mmproj" => ConfigValue::Path(rt.mmproj.clone()),
            "server_exe" => ConfigValue::Path(rt.server_exe.clone()),
            "dir" => ConfigValue::Path(rt.dir.clone()),
            "models_dir" => ConfigValue::Path(rt.models_dir.clone()),
            "hf_token" => ConfigValue::Secret(rt.hf_token.clone()),
            _ => return Err(unknown_key(&key)),
        };
        return Ok((key, value));
    }
    if let Some((group, field)) = key.split_once('.') {
        let params = match group {
            "writer" => &cfg.writer_params,
            "chat" => &cfg.chat_params,
            _ => return Err(unknown_key(&key)),
        };
        let value = match field {
            "thinking" => ConfigValue::Thinking(params.thinking),
            "temperature" => ConfigValue::Real(params.temperature),
            "max_tokens" => ConfigValue::Count(params.max_tokens),
            "top_p" => ConfigValue::Real(params.top_p),
            "top_k" => ConfigValue::Int(params.top_k),
            "min_p" => ConfigValue::Real(params.min_p),
            "seed" => ConfigValue::Seed(params.seed),
            _ => return Err(unknown_key(&key)),
        };
        return Ok((key, value));
    }
    let value = match key.as_str() {
        "bonsai_dir" => ConfigValue::Path(cfg.bonsai_dir.clone()),
        "server_url" => ConfigValue::Text(cfg.server_url.clone()),
        "auto_start" => ConfigValue::Bool(cfg.auto_start),
        "auto_stop_idle_minutes" => ConfigValue::Count(cfg.auto_stop_idle_minutes),
        "stop_on_exit" => ConfigValue::Bool(cfg.stop_on_exit),
        "vram_busy_mb" => ConfigValue::Count(cfg.vram_busy_mb),
        "ctx_size" => ConfigValue::Count(cfg.ctx_size),
        "mmproj_cpu" => ConfigValue::Bool(cfg.mmproj_cpu),
        "gpu_layers" => ConfigValue::Auto(cfg.gpu_layers),
        "image_max_tokens" => ConfigValue::Auto(cfg.image_max_tokens),
        "kv_cache_4bit" => ConfigValue::Bool(cfg.kv_cache_4bit),
        "extra_args" => ConfigValue::Args(cfg.extra_args.clone()),
        "start_timeout_secs" => ConfigValue::Count(cfg.start_timeout_secs),
        "default_duration" => ConfigValue::Count(cfg.default_duration),
        "save_history" => ConfigValue::Bool(cfg.save_history),
        "auto_describe" => ConfigValue::Bool(cfg.auto_describe),
        _ => return Err(unknown_key(&key)),
    };
    Ok((key, value))
}

/// Parse and store one setting; returns the normalised key.
fn config_set(cfg: &mut AppConfig, key: &str, value: &str) -> Result<String, CliError> {
    let (key, current) = config_get(cfg, key)?;
    let raw = value.trim();
    let bad = |what: &str| CliError::usage(format!("{key}: '{raw}' is not {what}"));
    let range_error = |e: String| CliError::usage(format!("{key}: {e}"));
    let parsed = match current {
        ConfigValue::Text(_) => {
            if raw.is_empty() || raw.contains(char::is_whitespace) {
                return Err(bad("a server URL like http://127.0.0.1:8080"));
            }
            ConfigValue::Text(raw.to_string())
        }
        ConfigValue::Path(_) => match raw.to_ascii_lowercase().as_str() {
            "" | "none" | "null" => ConfigValue::Path(None),
            _ => ConfigValue::Path(Some(absolute(Path::new(raw)))),
        },
        ConfigValue::Bool(_) => ConfigValue::Bool(parse_bool(raw).ok_or_else(|| bad("true or false"))?),
        ConfigValue::Count(_) => {
            let (min, max) = match key.as_str() {
                "default_duration" => (1, 120),
                k if k.ends_with("max_tokens") => (1, 131_072),
                "ctx_size" => (0, 1_048_576),
                _ => (0, u32::MAX),
            };
            ConfigValue::Count(parse_number(raw, min, max).map_err(range_error)?)
        }
        ConfigValue::Int(_) => ConfigValue::Int(parse_number(raw, 0, 1000).map_err(range_error)?),
        ConfigValue::Real(_) => {
            let max = if key.ends_with("temperature") { 2.0 } else { 1.0 };
            ConfigValue::Real(parse_number(raw, 0.0f32, max).map_err(range_error)?)
        }
        ConfigValue::Seed(_) => match raw.to_ascii_lowercase().as_str() {
            "" | "none" | "null" | "random" => ConfigValue::Seed(None),
            _ => ConfigValue::Seed(Some(raw.parse().map_err(|_| bad("a whole number or none"))?)),
        },
        ConfigValue::Thinking(_) => ConfigValue::Thinking(
            ThinkingMode::from_str(raw).map_err(|_| bad("a thinking level (off, low, medium, high)"))?,
        ),
        ConfigValue::Auto(_) => match raw.to_ascii_lowercase().as_str() {
            "" | "auto" | "none" | "default" => ConfigValue::Auto(None),
            _ => ConfigValue::Auto(Some(parse_number(raw, 0u32, 1_000_000).map_err(range_error)?)),
        },
        ConfigValue::Args(_) => ConfigValue::Args(raw.to_string()),
        ConfigValue::Secret(_) => match raw.to_ascii_lowercase().as_str() {
            "" | "none" | "null" => ConfigValue::Secret(None),
            // The masked form leaves the token as it is.
            "***" => ConfigValue::Secret(cfg.runtime.hf_token.clone()),
            _ => ConfigValue::Secret(Some(raw.to_string())),
        },
    };
    if let Some(field) = key.strip_prefix("runtime.") {
        let rt = &mut cfg.runtime;
        match (field, parsed) {
            ("model", ConfigValue::Path(p)) => rt.model = p,
            ("mmproj", ConfigValue::Path(p)) => rt.mmproj = p,
            ("server_exe", ConfigValue::Path(p)) => rt.server_exe = p,
            ("dir", ConfigValue::Path(p)) => rt.dir = p,
            ("models_dir", ConfigValue::Path(p)) => rt.models_dir = p,
            ("hf_token", ConfigValue::Secret(t)) => rt.hf_token = t,
            _ => return Err(unknown_key(&key)),
        }
        return Ok(key);
    }
    if let Some((group, field)) = key.split_once('.') {
        let params = if group == "writer" { &mut cfg.writer_params } else { &mut cfg.chat_params };
        match (field, parsed) {
            ("thinking", ConfigValue::Thinking(t)) => params.thinking = t,
            ("temperature", ConfigValue::Real(x)) => params.temperature = x,
            ("max_tokens", ConfigValue::Count(n)) => params.max_tokens = n,
            ("top_p", ConfigValue::Real(x)) => params.top_p = x,
            ("top_k", ConfigValue::Int(n)) => params.top_k = n,
            ("min_p", ConfigValue::Real(x)) => params.min_p = x,
            ("seed", ConfigValue::Seed(s)) => params.seed = s,
            _ => return Err(unknown_key(&key)),
        }
        return Ok(key);
    }
    match (key.as_str(), parsed) {
        ("bonsai_dir", ConfigValue::Path(p)) => cfg.bonsai_dir = p,
        ("server_url", ConfigValue::Text(s)) => cfg.server_url = s,
        ("auto_start", ConfigValue::Bool(b)) => cfg.auto_start = b,
        ("auto_stop_idle_minutes", ConfigValue::Count(n)) => cfg.auto_stop_idle_minutes = n,
        ("stop_on_exit", ConfigValue::Bool(b)) => cfg.stop_on_exit = b,
        ("vram_busy_mb", ConfigValue::Count(n)) => cfg.vram_busy_mb = n,
        ("ctx_size", ConfigValue::Count(n)) => cfg.ctx_size = n,
        ("mmproj_cpu", ConfigValue::Bool(b)) => cfg.mmproj_cpu = b,
        ("gpu_layers", ConfigValue::Auto(n)) => cfg.gpu_layers = n,
        ("image_max_tokens", ConfigValue::Auto(n)) => cfg.image_max_tokens = n,
        ("kv_cache_4bit", ConfigValue::Bool(b)) => cfg.kv_cache_4bit = b,
        ("extra_args", ConfigValue::Args(s)) => cfg.extra_args = s,
        ("start_timeout_secs", ConfigValue::Count(n)) => cfg.start_timeout_secs = n,
        ("default_duration", ConfigValue::Count(n)) => cfg.default_duration = n,
        ("save_history", ConfigValue::Bool(b)) => cfg.save_history = b,
        ("auto_describe", ConfigValue::Bool(b)) => cfg.auto_describe = b,
        _ => return Err(unknown_key(&key)),
    }
    Ok(key)
}

/// The config file for `config` / `install` commands: defaults when it does not exist, an error
/// when it exists but cannot be read (so a set never overwrites a file it could not parse).
fn load_config_strict() -> Result<AppConfig, CliError> {
    let path = config::config_path();
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    AppConfig::load_from(&path)
        .map_err(|e| CliError::fail(format!("{e:#} - fix the file or delete it to go back to the defaults")))
}

fn cmd_config(ctx: &Ctx, cmd: &ConfigCmd) -> CliResult {
    match cmd {
        ConfigCmd::Show => {
            let mut cfg = load_config_strict()?;
            if ctx.json {
                // The token is not printed.
                if cfg.runtime.hf_token.is_some() {
                    cfg.runtime.hf_token = Some("***".to_string());
                }
                print_json(&cfg)?;
                return Ok(EXIT_OK);
            }
            let mut lines = Vec::new();
            for key in CONFIG_KEYS {
                let (key, value) = config_get(&cfg, key)?;
                lines.push(format!("{key:<23} = {}", value.text()));
                if key == "bonsai_dir" && cfg.runtime.is_active() {
                    lines.push(format!("{:<23}   (not used while runtime.model is set)", ""));
                }
            }
            print_line(&lines.join("\n"));
            ctx.note(format!(
                "({}; change a setting with `bh3 config set <KEY> <VALUE>`)",
                config::config_path().display()
            ));
        }
        ConfigCmd::Path => {
            let path = config::config_path();
            if ctx.json {
                print_json(&json!({ "path": path, "dir": config::config_dir(), "exists": path.exists() }))?;
            } else {
                print_line(&path.display().to_string());
            }
        }
        ConfigCmd::Get(args) => {
            let cfg = load_config_strict()?;
            let (key, value) = config_get(&cfg, &args.key)?;
            if ctx.json {
                print_json(&json!({ "key": key, "value": value.json() }))?;
            } else {
                print_line(&value.text());
            }
        }
        ConfigCmd::Set(args) => {
            let mut cfg = load_config_strict()?;
            let key = config_set(&mut cfg, &args.key, &args.value)?;
            if key == "bonsai_dir" {
                if let Some(dir) = &cfg.bonsai_dir {
                    if let Err(e) = BonsaiInstall::detect(dir) {
                        ctx.warn(format!("{e:#} (saved anyway; `bh3 install set <DIR>` checks the folder first)"));
                    }
                }
            }
            cfg.save_to(&config::config_path()).map_err(CliError::from)?;
            let (key, value) = config_get(&cfg, &key)?;
            if ctx.json {
                print_json(&json!({ "key": key, "value": value.json() }))?;
            } else {
                print_line(&format!("{key} = {}", value.text()));
            }
        }
    }
    Ok(EXIT_OK)
}

#[derive(Debug, Serialize)]
struct DetectJson {
    /// `bonsai_dir` from the config.
    configured: Option<PathBuf>,
    installs: Vec<InstallJson>,
}

fn cmd_install(ctx: &Ctx, cmd: &InstallCmd) -> CliResult {
    match cmd {
        InstallCmd::Detect => {
            let found = server::find_installs();
            let report = DetectJson {
                configured: ctx.config().bonsai_dir.clone(),
                installs: found.iter().map(|i| InstallJson::of(i, None)).collect(),
            };
            if ctx.json {
                print_json(&report)?;
                return Ok(EXIT_OK);
            }
            let mut lines = Vec::new();
            if report.installs.is_empty() {
                lines.push(
                    "No Bonsai-demo folder found (looked on D:\\, C:\\, E:\\ and in your user folder).".to_string(),
                );
            } else {
                lines.push("Bonsai-demo folders found:".to_string());
                lines.extend(report.installs.iter().map(|install| format!("  {}", install.line())));
            }
            lines.push(match &report.configured {
                Some(dir) => format!("In use (config): {}", dir.display()),
                None => "None is set in the config yet: `bh3 install set <DIR>` saves one.".to_string(),
            });
            print_line(&lines.join("\n"));
            Ok(EXIT_OK)
        }
        InstallCmd::Set(args) => {
            let dir = absolute(&args.dir);
            let install = BonsaiInstall::detect(&dir).map_err(CliError::from)?;
            let mut cfg = load_config_strict()?;
            cfg.bonsai_dir = Some(install.root.clone());
            cfg.save_to(&config::config_path()).map_err(CliError::from)?;
            if ctx.json {
                print_json(&InstallJson::of(&install, Some(Source::Config)))?;
            } else {
                print_line(&format!("{}\nSaved as the Bonsai folder.", install.summary()));
            }
            if cfg.runtime.is_active() {
                ctx.warn(
                    "the app's own runtime (runtime.model) is set and is used instead of this folder - \
                     `bh3 models use none` switches back to the folder",
                );
            }
            for problem in install.problems() {
                ctx.warn(problem);
            }
            Ok(EXIT_OK)
        }
        InstallCmd::Show => {
            let Some(resolved) = ctx.resolve_install() else {
                return Err(CliError::fail(
                    "no Bonsai-demo folder is set and none was found: `bh3 install set <DIR>` sets one",
                ));
            };
            let report = InstallJson::of_resolved(&resolved);
            if ctx.json {
                print_json(&report)?;
            } else {
                match &resolved.detected {
                    Ok(install) => print_line(&format!("{}\n({})", install.summary(), resolved.source.label())),
                    Err(problem) => print_line(&format!(
                        "Bonsai folder: {} ({})\nNot usable: {problem}",
                        resolved.dir.display(),
                        resolved.source.label()
                    )),
                }
            }
            // Like `lint`: the report is printed either way, the exit code says whether it is usable.
            Ok(if report.usable { EXIT_OK } else { EXIT_FAIL })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// gpu / logs / agent-guide / version
// ---------------------------------------------------------------------------------------------

fn cmd_gpu(ctx: &Ctx) -> CliResult {
    let info = gpu::query().ok_or_else(|| {
        CliError::fail("no NVIDIA GPU status: nvidia-smi is missing or failed (it comes with the NVIDIA driver)")
    })?;
    let report = GpuJson::new(info, ctx.config().vram_busy_mb);
    if ctx.json {
        print_json(&report)?;
    } else {
        // The start hint only while no llama-server runs (it holds the memory itself).
        print_line(&report.lines(server::llama_server_pids().is_empty()).join("\n"));
    }
    Ok(EXIT_OK)
}

fn cmd_logs(ctx: &Ctx, args: &LogsArgs) -> CliResult {
    let path = server::default_log_path();
    let exists = path.is_file();
    let lines = server::read_log_tail(&path, args.lines);
    if ctx.json {
        print_json(&json!({ "path": path, "exists": exists, "lines": lines }))?;
    } else if !exists {
        ctx.note(format!("No server log yet: {} is written when bh3 or the app starts Bonsai.", path.display()));
    } else if !lines.is_empty() {
        print_line(&lines.join("\n"));
    }
    Ok(EXIT_OK)
}

fn cmd_agent_guide(ctx: &Ctx) -> CliResult {
    if ctx.json {
        print_json(&json!({ "guide": AGENT_GUIDE }))?;
    } else {
        print_line(AGENT_GUIDE);
    }
    Ok(EXIT_OK)
}

fn cmd_version(ctx: &Ctx) -> CliResult {
    if ctx.json {
        print_json(&json!({ "name": "bh3", "app": crate::APP_NAME, "version": crate::VERSION }))?;
    } else {
        print_line(&format!("bh3 {} ({})", crate::VERSION, crate::APP_NAME));
    }
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------------------------------------
// Input, files, clipboard, formatting
// ---------------------------------------------------------------------------------------------

impl GenArgs {
    /// `params` (the config's defaults) with the options on top; always streamed.
    fn apply(&self, mut params: GenParams) -> GenParams {
        if let Some(thinking) = self.thinking {
            params.thinking = thinking.into();
        }
        if let Some(t) = self.temperature {
            params.temperature = t;
        }
        if let Some(n) = self.max_tokens {
            params.max_tokens = n;
        }
        if let Some(p) = self.top_p {
            params.top_p = p;
        }
        if let Some(k) = self.top_k {
            params.top_k = k;
        }
        if let Some(p) = self.min_p {
            params.min_p = p;
        }
        if let Some(seed) = self.seed {
            params.seed = Some(seed);
        }
        params.stream = true;
        params
    }
}

/// Bytes to text: UTF-8 (BOM optional), UTF-16 LE/BE with BOM (what Windows PowerShell 5.1's `>`
/// writes); invalid UTF-8 is decoded lossily.
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

/// A text file, decoded and with `\n` line endings.
fn read_text_file(path: &Path) -> Result<String, CliError> {
    let bytes = std::fs::read(path).map_err(|e| CliError::fail(format!("cannot read {}: {e}", path.display())))?;
    Ok(normalize_text(&decode_text(&bytes)))
}

/// All of stdin; refused when stdin is a terminal (bh3 never waits for typing) or empty.
fn read_stdin_text(what: &str) -> Result<String, CliError> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(CliError::usage(format!(
            "{what} is read from stdin, but stdin is a terminal: pipe it in or use the file option"
        )));
    }
    let mut bytes = Vec::new();
    stdin.lock().read_to_end(&mut bytes).map_err(|e| CliError::fail(format!("reading stdin: {e}")))?;
    let text = normalize_text(&decode_text(&bytes));
    if text.trim().is_empty() {
        return Err(CliError::usage(format!("{what} is missing: stdin is empty (pipe it in or use the file option)")));
    }
    Ok(text)
}

/// Missing fields of `refs[]` and `dialogue[]` items get their defaults (`role` person, empty
/// texts, `send_image` false), so a hand-written spec only needs what it uses. The spec itself
/// has serde defaults for every field.
fn fill_spec_defaults(spec: &mut serde_json::Value) {
    let defaults: [(&str, &[(&str, serde_json::Value)]); 2] = [
        ("refs", &[("role", json!("person")), ("description", json!("")), ("send_image", json!(false))]),
        ("dialogue", &[("speaker", json!("")), ("words", json!(""))]),
    ];
    for (list, fields) in defaults {
        let Some(items) = spec.get_mut(list).and_then(serde_json::Value::as_array_mut) else { continue };
        for item in items.iter_mut().filter_map(serde_json::Value::as_object_mut) {
            for (key, value) in fields {
                item.entry(key.to_string()).or_insert_with(|| value.clone());
            }
        }
    }
}

/// A RequestSpec JSON file. Relative picture paths are relative to the file's folder.
fn read_spec(path: &Path) -> Result<RequestSpec, CliError> {
    let invalid = |e: serde_json::Error| {
        CliError::usage(format!(
            "{} is not a valid request spec (RequestSpec JSON, see docs/CLI.md): {e}",
            path.display()
        ))
    };
    let text = read_text_file(path)?;
    let mut value: serde_json::Value = serde_json::from_str(&text).map_err(invalid)?;
    fill_spec_defaults(&mut value);
    let mut spec: RequestSpec = serde_json::from_value(value).map_err(invalid)?;
    // Absolute, so the history keeps paths that work from any folder (`bh3 fix`, the app).
    let base = absolute(path).parent().map(Path::to_path_buf).unwrap_or_default();
    for reference in &mut spec.refs {
        if let Some(image) = reference.image_path.as_mut() {
            if image.is_relative() {
                *image = base.join(&*image);
            }
        }
    }
    Ok(spec)
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| format!("cannot open the clipboard: {e}"))?;
    clipboard.set_text(text).map_err(|e| format!("cannot copy to the clipboard: {e}"))
}

/// `path` made absolute (against the current folder) so saved folders work from anywhere.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `6` for 6.0, `6.5` for 6.5, `0.4` for 0.4f32.
fn fmt_num(x: f32) -> String {
    let rounded = round3(x);
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}")
    } else {
        rounded.to_string()
    }
}

/// `f32` to `f64` without the binary tail (`0.4`, not `0.4000000059604645`).
fn round3(x: f32) -> f64 {
    (f64::from(x) * 1000.0).round() / 1000.0
}

/// `48 s`, `2 min 05 s`.
fn fmt_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{} min {:02} s", secs / 60, secs % 60)
    }
}

/// `1 error`, `2 errors`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn join_display<T: std::fmt::Display>(items: &[T], sep: &str) -> String {
    items.iter().map(ToString::to_string).collect::<Vec<_>>().join(sep)
}

// ---------------------------------------------------------------------------------------------
// Ctrl+C
// ---------------------------------------------------------------------------------------------

#[cfg(windows)]
mod console {
    use std::sync::atomic::Ordering;
    use std::sync::Once;

    type HandlerRoutine = extern "system" fn(ctrl_type: u32) -> i32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
    }

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;

    extern "system" fn on_ctrl(ctrl_type: u32) -> i32 {
        if ctrl_type != CTRL_C_EVENT && ctrl_type != CTRL_BREAK_EVENT {
            return 0;
        }
        // The first Ctrl+C sets the cancel flag (handled); a second one goes on to the default
        // handler, which ends bh3.
        i32::from(!super::CANCEL.swap(true, Ordering::SeqCst))
    }

    /// Route Ctrl+C to the cancel flag (once per process).
    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: `on_ctrl` has the HandlerRoutine signature and only touches an atomic; it
            // stays valid for the whole process.
            unsafe {
                SetConsoleCtrlHandler(Some(on_ctrl), 1);
            }
        });
    }

    extern "system" fn swallow(ctrl_type: u32) -> i32 {
        i32::from(ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT)
    }

    /// For the MCP server: Ctrl+C / Ctrl+Break of a shared console only cancel the running call
    /// and never end the process (its client ends it by closing stdin). Call before [`install`]:
    /// handlers added later run first.
    pub fn keep_alive() {
        // SAFETY: `swallow` has the HandlerRoutine signature and touches nothing; it stays valid
        // for the whole process.
        unsafe {
            SetConsoleCtrlHandler(Some(swallow), 1);
        }
        install();
    }
}

#[cfg(not(windows))]
mod console {
    /// Ctrl+C keeps its default behaviour outside Windows.
    pub fn install() {}

    /// Ctrl+C keeps its default behaviour outside Windows.
    pub fn keep_alive() {}
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("bh3").chain(args.iter().copied()))
    }

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn write_needs_exactly_one_input_and_a_mode() {
        assert!(parse(&["write", "--mode", "r2v", "--request", "Ref 1: a woman."]).is_ok());
        assert!(parse(&["write", "--spec-file", "form.json"]).is_ok(), "the spec gives the mode");
        let missing = parse(&["write", "--mode", "r2v"]).unwrap_err();
        assert_eq!(missing.exit_code(), EXIT_USAGE);
        assert!(parse(&["write", "--request", "x"]).is_err(), "no mode");
        assert!(parse(&["write", "--mode", "r2v", "--request", "x", "--stdin"]).is_err());
        assert!(parse(&["write", "--mode", "bogus", "--request", "x"]).is_err());
        // Aliases, and global options after the subcommand.
        let cli = parse(&["write", "--mode", "fl2va", "--stdin", "--json", "--thinking", "xhigh"]).unwrap();
        assert!(cli.global.json);
        match cli.command {
            Cmd::Write(args) => {
                assert_eq!(args.mode, Some(ModeArg::F2v));
                assert_eq!(args.gen.thinking, Some(ThinkingArg::High));
                assert!(args.stdin);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn optional_bool_flags() {
        let ctx_of = |args: &[&str]| match parse(args).unwrap().command {
            Cmd::Lint(lint) => lint.context,
            other => panic!("{other:?}"),
        };
        let c = ctx_of(&["lint", "--pinned", "--last-frame=false", "--refs", "2", "--duration", "6.5"]);
        assert_eq!((c.pinned, c.last_frame, c.refs, c.duration), (Some(true), Some(false), Some(2), Some(6.5)));
        assert_eq!(ctx_of(&["lint", "--pinned=no"]).pinned, Some(false));
        assert_eq!(ctx_of(&["lint"]).pinned, None);
        assert!(parse(&["lint", "--refs", "10"]).is_err());
        assert!(parse(&["lint", "--duration", "0"]).is_err());
        assert!(parse(&["lint", "--pinned=maybe"]).is_err());
    }

    #[test]
    fn fix_needs_an_entry_and_seeds_take_negative_numbers() {
        assert!(parse(&["fix", "--last", "--fix", "Make it longer."]).is_ok());
        assert!(parse(&["fix", "--id", "x", "--from-lint"]).is_ok());
        assert!(parse(&["fix", "--fix", "Make it longer."]).is_err(), "no entry");
        assert!(parse(&["fix", "--last"]).is_err(), "no change");
        assert!(parse(&["fix", "--last", "--fix", "y", "--from-lint"]).is_err());
        assert!(parse(&["fix", "--last", "--id", "x", "--fix", "y"]).is_err());
        match parse(&["chat", "--message", "hi", "--seed", "-1", "--system", "file:sys.txt"]).unwrap().command {
            Cmd::Chat(chat) => {
                assert_eq!(chat.gen.seed, Some(-1));
                assert_eq!(chat.system, SystemArg::File(PathBuf::from("sys.txt")));
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(&["config", "set", "writer.seed", "-5"]).is_ok());
        assert!(parse(&["fixes", "--mode", "f2v"]).is_ok());
        assert!(parse(&["fixes", "--mode", "t2v"]).is_err());
        assert!(parse(&["mcp"]).is_ok());
    }

    #[test]
    fn spec_pictures_get_absolute_paths() {
        // A relative --spec-file (tests run in the crate folder): its pictures are saved with
        // absolute paths, so `bh3 fix` from another folder still finds them.
        let dir = Path::new("target").join(format!("bh3-spec-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("form.json");
        std::fs::write(
            &file,
            r#"{"refs": [{"description": "a woman", "image_path": "ref1.png", "send_image": true}]}"#,
        )
        .unwrap();
        let spec = read_spec(&file);
        let _ = std::fs::remove_dir_all(&dir);
        let image = spec.unwrap().refs[0].image_path.clone().unwrap();
        assert!(image.is_absolute(), "{}", image.display());
        assert_eq!(image, absolute(&dir).join("ref1.png"));
    }

    #[test]
    fn system_prompt_choices() {
        assert_eq!(parse_system("none"), Ok(SystemArg::None));
        assert_eq!(parse_system("R2V"), Ok(SystemArg::Mode(Mode::R2v)));
        assert_eq!(parse_system("i2v"), Ok(SystemArg::Mode(Mode::F2v)));
        assert_eq!(parse_system("file: my prompt.txt"), Ok(SystemArg::File(PathBuf::from("my prompt.txt"))));
        assert!(parse_system("file:").is_err());
        assert!(parse_system("pirate").is_err());
    }

    #[test]
    fn context_options_override_and_link_f2v_facts() {
        let base = request::context_from_text(Mode::F2v, "Picture 1: a man. 6 s: he waves.");
        let opts = ContextArgs { refs: Some(2), ..ContextArgs::default() };
        let c = opts.apply(base.clone()).unwrap();
        assert_eq!((c.ref_count, c.has_last_frame), (Some(2), Some(true)));
        let opts = ContextArgs { last_frame: Some(false), duration: Some(3.0), ..ContextArgs::default() };
        let c = opts.apply(base.clone()).unwrap();
        assert_eq!((c.ref_count, c.has_last_frame, c.duration_s), (Some(1), Some(false), Some(3.0)));
        let too_many = ContextArgs { refs: Some(3), ..ContextArgs::default() };
        assert_eq!(too_many.apply(base).unwrap_err().code, EXIT_USAGE);

        let partial = LintContext { mode: Mode::R2v, duration_s: Some(8.0), ..LintContext::default() };
        let filled = request::fill_missing(
            partial,
            &request::context_from_text(Mode::R2v, "Ref 1: a woman. Ref 2: a cafe. 6 s."),
        );
        assert_eq!((filled.duration_s, filled.ref_count, filled.first_frame_pinned), (Some(8.0), Some(2), Some(false)));
    }

    #[test]
    fn config_keys_round_trip() {
        let mut cfg = AppConfig::default();
        for key in CONFIG_KEYS {
            let (normalized, value) = config_get(&cfg, key).unwrap();
            assert_eq!(normalized, key);
            // Setting a value to its own text form changes nothing.
            let text = value.text();
            config_set(&mut cfg, key, &text).unwrap_or_else(|e| panic!("{key} = {text}: {e:?}"));
        }
        assert_eq!(cfg, AppConfig::default());

        assert_eq!(config_set(&mut cfg, "Writer_Params.Temperature", "0.3").unwrap(), "writer.temperature");
        assert_eq!(cfg.writer_params.temperature, 0.3);
        assert_eq!(config_get(&cfg, "writer.temperature").unwrap().1.json(), json!(0.3));
        assert_eq!(config_set(&mut cfg, "Chat.Top-P", "0.9").unwrap(), "chat.top_p");
        config_set(&mut cfg, "chat.thinking", "xhigh").unwrap();
        assert_eq!(cfg.chat_params.thinking, ThinkingMode::High);
        config_set(&mut cfg, "auto_start", "off").unwrap();
        assert!(!cfg.auto_start);
        config_set(&mut cfg, "writer.seed", "42").unwrap();
        assert_eq!(cfg.writer_params.seed, Some(42));
        config_set(&mut cfg, "writer.seed", "random").unwrap();
        assert_eq!(cfg.writer_params.seed, None);
        config_set(&mut cfg, "bonsai_dir", r"D:\Bonsai-demo").unwrap();
        assert!(cfg.bonsai_dir.is_some());
        config_set(&mut cfg, "bonsai_dir", "none").unwrap();
        assert_eq!(cfg.bonsai_dir, None);

        for (key, value) in [
            ("nope", "1"),
            ("writer.nope", "1"),
            ("auto_start", "maybe"),
            ("writer.temperature", "3"),
            ("writer.top_p", "-0.1"),
            ("chat.max_tokens", "0"),
            ("server_url", "  "),
            ("default_duration", "x"),
            ("writer.thinking", "extreme"),
        ] {
            let err = config_set(&mut cfg, key, value).unwrap_err();
            assert_eq!(err.code, EXIT_USAGE, "{key} = {value}");
        }
    }

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi \xE2\x80\x94 there"), "hi \u{2014} there");
        assert_eq!(decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode_text(&[0xFE, 0xFF, 0, b'h', 0, b'i']), "hi");
        assert_eq!(decode_text(b"plain"), "plain");
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_num(6.0), "6");
        assert_eq!(fmt_num(6.5), "6.5");
        assert_eq!(fmt_num(0.4), "0.4");
        assert_eq!(round3(0.4), 0.4);
        assert_eq!(fmt_elapsed(Duration::from_secs(48)), "48 s");
        assert_eq!(fmt_elapsed(Duration::from_secs(125)), "2 min 05 s");
        assert_eq!(plural(1, "error"), "1 error");
        assert_eq!(plural(0, "error"), "0 errors");
        assert_eq!(exit_kind(EXIT_UNAVAILABLE), "unavailable");
        assert_eq!(exit_kind(42), "failure");
        let row = LengthRow::of(6.0);
        assert_eq!((row.frames, row.real.as_str(), row.real_s, row.max_cuts), (158, "6.58", 6.58, 2));
        assert_eq!(row.max_megapixels, 0.5);
    }

    #[test]
    fn fixes_from_the_check_list_errors_and_warnings_once() {
        let raw = include_str!("../../tests/fixtures/bad_r2v_markdown_fake_shots.txt");
        let ctx = LintContext { mode: Mode::R2v, duration_s: Some(15.0), ref_count: Some(2), ..LintContext::default() };
        let report = lint::lint(raw, &ctx);
        let fix = fix_from_report(&report).unwrap();
        assert!(fix.starts_with("Fix these problems and keep everything else the same:\n- "), "{fix}");
        let listed = fix.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(listed, report.errors() + report.warnings());
        assert!(!fix.contains("No subtitles"), "info issues are left out: {fix}");

        let good = include_str!("../../tests/fixtures/good_f2v_e8.txt");
        let ctx = LintContext {
            mode: Mode::F2v,
            duration_s: Some(3.0),
            ref_count: Some(2),
            has_last_frame: Some(true),
            ..LintContext::default()
        };
        let report = lint::lint(good, &ctx);
        assert_eq!(report.errors() + report.warnings(), 0, "{:?}", report.issues);
        assert_eq!(fix_from_report(&report), None);
    }

    #[test]
    fn empty_answers_explain_the_token_limit() {
        let off = GenParams::prompt_writer();
        assert_eq!(empty_answer_message("stop", &off), "Bonsai returned an empty answer (finish_reason stop)");
        assert!(empty_answer_message("length", &off).contains("raise --max-tokens (now 1500)"));
        let thinking = GenParams { thinking: ThinkingMode::High, ..off };
        assert!(empty_answer_message("length", &thinking).contains("--thinking off"));
        assert!(empty_answer_message("", &thinking).contains("finish_reason none"));
    }

    #[test]
    fn stream_printer_announces_phases_once() {
        let mut printer = StreamPrinter::new(true, false);
        printer.piece(&StreamEvent::Reasoning("hmm".into()));
        assert_eq!(printer.phase, None, "quiet: no notes");
        let mut printer = StreamPrinter::new(false, false);
        printer.piece(&StreamEvent::Content("  ".into()));
        assert_eq!(printer.phase, None);
        printer.piece(&StreamEvent::Reasoning("think".into()));
        assert_eq!(printer.phase, Some(PieceKind::Reasoning));
        printer.piece(&StreamEvent::Content("subject_definitions:".into()));
        assert_eq!(printer.phase, Some(PieceKind::Content));
        printer.piece(&StreamEvent::Reasoning("late".into()));
        assert_eq!(printer.phase, Some(PieceKind::Content), "phases only move forward");
    }
}
