//! `bh3 mcp`: a Model Context Protocol server on stdin/stdout, so Claude Code and other MCP
//! clients can use the writer as tools (see `docs/MCP.md`).
//!
//! - Transport: newline-delimited JSON-RPC 2.0, one UTF-8 message per line. A reader thread parses
//!   stdin and passes the messages to the main thread, which handles them one at a time. While a
//!   tool call runs, the reader turns a `notifications/cancelled` for it into the CLI's cancel flag
//!   (a cancel that arrives before its call started is remembered). The end of stdin ends the
//!   server (exit 0) once the running call and the queued messages are done.
//! - Only protocol messages go to stdout. The tools reuse the CLI's functions, which write their
//!   progress and notes to stderr (MCP clients log it).
//! - Tool failures (Bonsai not running, a bad argument value, an unknown history id) are tool
//!   results with `isError: true`; an unknown tool or arguments that are not an object are
//!   JSON-RPC errors (-32602).
//! - No tool stops Bonsai except `stop_bonsai`; a Bonsai that a tool started keeps running.

use std::collections::VecDeque;
use std::io::BufRead;
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};

use super::*;
use crate::client::Timings;

/// Protocol versions this server speaks.
const PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
/// The answer to a client that asks for a version not in [`PROTOCOL_VERSIONS`].
const LATEST_PROTOCOL: &str = "2025-06-18";

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

/// Cancellations of calls that have not started yet, remembered at most.
const MAX_EARLY_CANCELS: usize = 32;
/// Most entries `list_history` returns.
const MAX_LIST_LIMIT: u64 = 200;

/// `initialize` instructions for the agent.
const INSTRUCTIONS: &str = "Bonsai H3 Writer writes MiniMax H3 video prompts with the user's local Bonsai 27B LLM \
(llama-server on this PC). write_prompt starts Bonsai when it is needed (loading the model takes about a minute) and \
returns the prompt plus a check report against the official H3 format. Fix problems with fix_prompt (from_lint: true, \
or your own words in fix) instead of editing the prompt yourself, and give the user the final prompt unchanged: it is \
pasted into MiniMax H3 as is. Leave thinking \"off\", the tested default. Bonsai keeps running after a call; \
stop_bonsai frees the 8 GB GPU for ComfyUI, and it also stops a Bonsai the user started.";

/// Run the MCP server until stdin ends; returns the exit code (0, or 1 when stdin cannot be read).
pub(super) fn serve(ctx: &Ctx) -> i32 {
    let interactive = std::io::stdin().is_terminal();
    if !interactive {
        console::keep_alive();
    }
    let server = Server::new(ctx);
    let (tx, rx) = mpsc::channel();
    let calls = Arc::clone(&server.calls);
    let reader = std::thread::Builder::new().name("mcp-stdin".to_string()).spawn(move || read_stdin(&tx, &calls));
    if let Err(e) = reader {
        eprint_line(&format!("error: cannot read stdin: {e}"));
        return EXIT_FAIL;
    }
    ctx.note(format!(
        "bh3 mcp {}: MCP server on stdin/stdout for Bonsai at {} (notes on stderr).",
        crate::VERSION,
        ctx.url()
    ));
    if interactive {
        ctx.note(
            "stdin is a terminal: MCP clients start `bh3 mcp` themselves (docs/MCP.md). Type one JSON-RPC message \
             per line; Ctrl+Z and Enter end it.",
        );
    }
    for line in rx {
        if let Some(reply) = server.handle(line) {
            send(&reply);
        }
    }
    EXIT_OK
}

/// One message on stdout; a closed pipe is ignored (the client is gone).
fn send(reply: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(reply.as_bytes()).and_then(|()| out.write_all(b"\n")).and_then(|()| out.flush());
}

// ---------------------------------------------------------------------------------------------
// Reading stdin, cancellation
// ---------------------------------------------------------------------------------------------

/// One line of stdin.
enum Line {
    Message(Value),
    /// Not JSON (or not UTF-8).
    Malformed,
}

/// A stdin line as a message; `None` for a blank line.
fn parse_line(bytes: &[u8]) -> Option<Line> {
    let Ok(text) = std::str::from_utf8(bytes) else { return Some(Line::Malformed) };
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return None;
    }
    Some(serde_json::from_str(text).map_or(Line::Malformed, Line::Message))
}

/// The reader thread: stdin lines to `tx`, cancellations to `calls` at once. Ends with stdin.
fn read_stdin(tx: &mpsc::Sender<Line>, calls: &Mutex<Calls>) {
    let mut input = std::io::stdin().lock();
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match input.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Some(line) = parse_line(&bytes) else { continue };
        if let Line::Message(message) = &line {
            for id in cancelled_ids(message) {
                lock(calls).cancel(id);
            }
        }
        if tx.send(line).is_err() {
            return;
        }
    }
}

/// Request ids of the `notifications/cancelled` in a message or batch.
fn cancelled_ids(message: &Value) -> Vec<&Value> {
    let items = match message {
        Value::Array(items) => items.as_slice(),
        one => std::slice::from_ref(one),
    };
    items
        .iter()
        .filter(|m| m.get("method").and_then(Value::as_str) == Some("notifications/cancelled") && m.get("id").is_none())
        .filter_map(|m| m.get("params")?.get("requestId"))
        .collect()
}

/// The running tool call, and cancellations that arrived before their call started.
struct Calls {
    /// Set to cancel the running call (the CLI's Ctrl+C flag).
    flag: &'static AtomicBool,
    running: Option<Value>,
    early: VecDeque<Value>,
}

impl Calls {
    fn new(flag: &'static AtomicBool) -> Self {
        Self { flag, running: None, early: VecDeque::new() }
    }

    /// A `notifications/cancelled` for request `id`: cancel the running call, or remember it.
    fn cancel(&mut self, id: &Value) {
        if self.running.as_ref() == Some(id) {
            self.flag.store(true, Ordering::SeqCst);
        } else if !self.early.contains(id) {
            if self.early.len() == MAX_EARLY_CANCELS {
                self.early.pop_front();
            }
            self.early.push_back(id.clone());
        }
    }

    /// Call `id` starts (the cancel flag is cleared); false when it was cancelled already.
    fn begin(&mut self, id: &Value) -> bool {
        if let Some(i) = self.early.iter().position(|e| e == id) {
            self.early.remove(i);
            return false;
        }
        self.flag.store(false, Ordering::SeqCst);
        self.running = Some(id.clone());
        true
    }

    fn end(&mut self) {
        self.running = None;
    }
}

fn lock(calls: &Mutex<Calls>) -> MutexGuard<'_, Calls> {
    calls.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------------------------

/// The protocol side: messages in, replies out (no I/O, so it can be tested directly).
struct Server<'a> {
    /// The global options; every tool call runs with a fresh copy ([`Ctx::reloaded`]).
    base: &'a Ctx,
    calls: Arc<Mutex<Calls>>,
    tools: Vec<Tool>,
}

impl<'a> Server<'a> {
    fn new(base: &'a Ctx) -> Self {
        Self { base, calls: Arc::new(Mutex::new(Calls::new(&CANCEL))), tools: tools() }
    }

    /// [`Self::handle`] for a raw line.
    #[cfg(test)]
    fn handle_text(&self, text: &str) -> Option<String> {
        parse_line(text.as_bytes()).and_then(|line| self.handle(line))
    }

    /// The reply to one line (a message or a batch); `None` when nothing is answered.
    fn handle(&self, line: Line) -> Option<String> {
        let message = match line {
            Line::Malformed => {
                return Some(error_reply(&Value::Null, PARSE_ERROR, "Parse error: send one JSON-RPC message per line"))
            }
            Line::Message(message) => message,
        };
        match message {
            Value::Array(items) if items.is_empty() => {
                Some(error_reply(&Value::Null, INVALID_REQUEST, "Invalid request: an empty batch"))
            }
            Value::Array(items) => {
                let replies: Vec<String> = items.into_iter().filter_map(|item| self.handle_one(item)).collect();
                (!replies.is_empty()).then(|| format!("[{}]", replies.join(",")))
            }
            one => self.handle_one(one),
        }
    }

    /// Requests get exactly one reply; notifications and responses get none.
    fn handle_one(&self, message: Value) -> Option<String> {
        let invalid = |id: &Value| {
            error_reply(
                id,
                INVALID_REQUEST,
                "Invalid request: send {\"jsonrpc\": \"2.0\", \"id\": <number or string>, \"method\": ..., \"params\": {...}}",
            )
        };
        let Value::Object(message) = message else { return Some(invalid(&Value::Null)) };
        let method = message.get("method").and_then(Value::as_str);
        if method.is_none() && (message.contains_key("result") || message.contains_key("error")) {
            return None; // a response: this server sends no requests
        }
        let id = message.get("id").filter(|id| !id.is_null());
        let jsonrpc = message.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
        match (method, id) {
            // Notifications are not answered; the reader thread handles cancellations.
            (Some(_), None) => None,
            (Some(method), Some(id)) if is_id(id) && jsonrpc => Some(self.request(method, id, message.get("params"))),
            (_, id) => Some(invalid(id.filter(|id| is_id(id)).unwrap_or(&Value::Null))),
        }
    }

    fn request(&self, method: &str, id: &Value, params: Option<&Value>) -> String {
        let result = match method {
            "initialize" => Ok(initialize_result(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools.iter().map(Tool::definition).collect::<Vec<_>>() })),
            "tools/call" => self.call(id, params),
            other => Err((METHOD_NOT_FOUND, format!("Method not found: {other}"))),
        };
        match result {
            Ok(result) => reply_text(&json!({ "jsonrpc": "2.0", "id": id, "result": result })),
            Err((code, message)) => error_reply(id, code, &message),
        }
    }

    /// `tools/call`: the tool's result (failures included), or a JSON-RPC error for an unknown
    /// tool or arguments that are not an object.
    fn call(&self, id: &Value, params: Option<&Value>) -> Result<Value, (i32, String)> {
        let params = params.and_then(Value::as_object);
        let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
            return Err((
                INVALID_PARAMS,
                "Invalid params: tools/call needs {\"name\": ..., \"arguments\": {...}}".into(),
            ));
        };
        let Some(tool) = self.tools.iter().find(|t| t.name == name) else {
            return Err((INVALID_PARAMS, format!("Invalid params: unknown tool '{name}' (see tools/list)")));
        };
        let none = Map::new();
        let arguments = match params.and_then(|p| p.get("arguments")) {
            None | Some(Value::Null) => &none,
            Some(Value::Object(arguments)) => arguments,
            Some(_) => {
                return Err((INVALID_PARAMS, format!("Invalid params: the arguments of {name} must be an object")))
            }
        };
        if !lock(&self.calls).begin(id) {
            return Ok(ToolOutput::failure("cancelled", "cancelled before it started").result());
        }
        let mut ctx = self.base.reloaded();
        let output = panic::catch_unwind(AssertUnwindSafe(|| {
            ToolArgs::check(tool, arguments).and_then(|args| (tool.run)(&mut ctx, &args))
        }));
        lock(&self.calls).end();
        match output {
            Ok(Ok(output)) => Ok(output.result()),
            Ok(Err(err)) => Ok(ToolOutput::error(&err).result()),
            Err(_) => Err((INTERNAL_ERROR, format!("Internal error: {name} crashed (the message is on stderr)"))),
        }
    }
}

fn is_id(id: &Value) -> bool {
    id.is_string() || id.is_number()
}

/// The client's protocol version when this server speaks it, else the newest one.
fn negotiate(requested: Option<&str>) -> &'static str {
    PROTOCOL_VERSIONS.iter().copied().find(|v| Some(*v) == requested).unwrap_or(LATEST_PROTOCOL)
}

fn initialize_result(params: Option<&Value>) -> Value {
    let requested = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str);
    json!({
        "protocolVersion": negotiate(requested),
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "bonsai-h3-writer", "title": crate::APP_NAME, "version": crate::VERSION },
        "instructions": INSTRUCTIONS,
    })
}

fn reply_text(reply: &Value) -> String {
    serde_json::to_string(reply).unwrap_or_default()
}

fn error_reply(id: &Value, code: i32, message: &str) -> String {
    reply_text(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
}

/// `data` as JSON with `f32` numbers as written (`0.4`, not `0.4000000059604645`): serialised to
/// text first, like [`print_json`] does.
fn value_of<T: Serialize + ?Sized>(data: &T) -> Value {
    serde_json::to_string(data).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// The CLI's hints in a message, said with tool names (`bh3 start --wait` -> start_bonsai).
fn tool_text(message: &str) -> String {
    const WORDS: [(&str, &str); 12] = [
        (
            "`bh3 start --force --wait` and run this command again",
            "the start_bonsai tool (force: true) and call this tool again",
        ),
        ("`bh3 start --wait`", "start_bonsai"),
        ("`bh3 write` starts", "write_prompt starts"),
        ("Check with `bh3 status` (or use --wait)", "Check with bonsai_status (or call start_bonsai with wait: true)"),
        ("`bh3 status`", "bonsai_status"),
        ("(see `bh3 history list`)", "(see list_history)"),
        ("bh3 write saves every prompt it writes", "write_prompt saves every prompt it writes"),
        ("use --thinking off or raise --max-tokens", "use thinking \"off\" or raise max_tokens"),
        ("raise --max-tokens", "raise max_tokens"),
        ("--refs ", "refs "),
        ("(--no-auto-start)", "(auto_start false)"),
        ("cancelled (Ctrl+C)", "cancelled"),
    ];
    WORDS.iter().fold(message.to_string(), |text, (cli, tool)| text.replace(cli, tool))
}

// ---------------------------------------------------------------------------------------------
// Tools: definitions and arguments
// ---------------------------------------------------------------------------------------------

/// What a tool returns: text for the agent, and the facts as JSON (`structuredContent`).
struct ToolOutput {
    text: String,
    data: Value,
    is_error: bool,
}

impl ToolOutput {
    fn ok(text: impl Into<String>, data: Value) -> Self {
        Self { text: text.into(), data, is_error: false }
    }

    /// A failed call: the message (the CLI's hints said with tool names) and its kind (`unavailable` ...).
    fn error(err: &CliError) -> Self {
        Self::failure(exit_kind(err.code), &tool_text(&err.message))
    }

    fn failure(kind: &str, message: &str) -> Self {
        Self { text: message.to_string(), data: json!({ "error": message, "kind": kind }), is_error: true }
    }

    /// The `tools/call` result.
    fn result(self) -> Value {
        json!({
            "content": [{ "type": "text", "text": self.text }],
            "structuredContent": self.data,
            "isError": self.is_error,
        })
    }
}

/// A tool's function.
type ToolFn = fn(&mut Ctx, &ToolArgs<'_>) -> Result<ToolOutput, CliError>;

/// One tool: what `tools/list` shows, and its function.
struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// JSON Schema of each argument (also the list of known arguments).
    properties: Value,
    required: &'static [&'static str],
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    run: ToolFn,
}

impl Tool {
    /// The entry in `tools/list`.
    fn definition(&self) -> Value {
        let mut schema = json!({ "type": "object", "properties": self.properties, "additionalProperties": false });
        if !self.required.is_empty() {
            schema["required"] = json!(self.required);
        }
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": schema,
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.idempotent,
                "openWorldHint": false,
            },
        })
    }
}

/// The arguments of one call. [`ToolArgs::check`] refuses unknown and missing ones; the getters
/// check the types (`null` counts as not given; numbers and booleans may also come as strings).
struct ToolArgs<'a> {
    tool: &'static str,
    map: &'a Map<String, Value>,
}

impl<'a> ToolArgs<'a> {
    fn check(tool: &Tool, map: &'a Map<String, Value>) -> Result<Self, CliError> {
        let known = tool.properties.as_object();
        if let Some(key) = map.keys().find(|key| !known.is_some_and(|k| k.contains_key(*key))) {
            let names: Vec<&str> = known.map(|k| k.keys().map(String::as_str).collect()).unwrap_or_default();
            let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
            return Err(CliError::usage(format!("{}: unknown argument '{key}' (arguments: {names})", tool.name)));
        }
        let args = Self { tool: tool.name, map };
        if let Some(key) = tool.required.iter().find(|key| args.get(key).is_none()) {
            return Err(CliError::usage(format!("{}: '{key}' is required", tool.name)));
        }
        Ok(args)
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    fn bad(&self, key: &str, what: &str) -> CliError {
        CliError::usage(format!("{}: '{key}' must be {what}", self.tool))
    }

    fn text(&self, key: &str) -> Result<Option<&'a str>, CliError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(self.bad(key, "a string")),
        }
    }

    fn flag(&self, key: &str) -> Result<Option<bool>, CliError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(Value::String(text)) if parse_bool(text).is_some() => Ok(parse_bool(text)),
            Some(_) => Err(self.bad(key, "true or false")),
        }
    }

    fn number(&self, key: &str, min: f64, max: f64) -> Result<Option<f64>, CliError> {
        let value = match self.get(key) {
            None => return Ok(None),
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(text)) => text.trim().parse().ok(),
            Some(_) => None,
        };
        match value {
            Some(x) if (min..=max).contains(&x) => Ok(Some(x)),
            _ => Err(self.bad(key, &format!("a number from {min} to {max}"))),
        }
    }

    fn integer(&self, key: &str, min: u64, max: u64) -> Result<Option<u64>, CliError> {
        match self.number(key, min as f64, max as f64) {
            Ok(None) => Ok(None),
            Ok(Some(x)) if x.fract() == 0.0 => Ok(Some(x as u64)),
            _ => Err(self.bad(key, &format!("a whole number from {min} to {max}"))),
        }
    }

    /// A clip length: more than 0 and at most 120 seconds (like `--duration`).
    fn seconds(&self, key: &str) -> Result<Option<f32>, CliError> {
        match self.number(key, 0.0, 120.0) {
            Ok(None) => Ok(None),
            Ok(Some(x)) if x > 0.0 => Ok(Some(x as f32)),
            _ => Err(self.bad(key, "seconds, more than 0 and at most 120")),
        }
    }

    fn texts(&self, key: &str) -> Result<Vec<&'a str>, CliError> {
        let bad = || self.bad(key, "a list of strings");
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items.iter().map(|item| item.as_str().ok_or_else(bad)).collect(),
            Some(_) => Err(bad()),
        }
    }
}

/// `r2v` / `f2v` (the CLI's aliases work too).
fn mode_arg(args: &ToolArgs<'_>, key: &str) -> Result<Option<Mode>, CliError> {
    let Some(text) = args.text(key)? else { return Ok(None) };
    <ModeArg as ValueEnum>::from_str(text.trim(), true)
        .map(|mode| Some(mode.into()))
        .map_err(|_| args.bad(key, "\"r2v\" or \"f2v\""))
}

/// `thinking`, `temperature`, `max_tokens` (the rest comes from the config).
fn gen_args(args: &ToolArgs<'_>) -> Result<GenArgs, CliError> {
    let thinking = match args.text("thinking")? {
        None => None,
        Some(text) => Some(
            <ThinkingArg as ValueEnum>::from_str(text.trim(), true)
                .map_err(|_| args.bad("thinking", "\"off\", \"low\", \"medium\" or \"high\""))?,
        ),
    };
    Ok(GenArgs {
        thinking,
        temperature: args.number("temperature", 0.0, 2.0)?.map(|t| t as f32),
        max_tokens: args.integer("max_tokens", 256, 16_384)?.map(|n| n as u32),
        ..GenArgs::default()
    })
}

/// The check facts `duration_s`, `refs`, `first_frame_pinned`, `has_last_frame`.
fn context_args(args: &ToolArgs<'_>) -> Result<ContextArgs, CliError> {
    Ok(ContextArgs {
        duration: args.seconds("duration_s")?,
        refs: args.integer("refs", 1, request::MAX_R2V_REFS as u64)?.map(|n| n as usize),
        pinned: args.flag("first_frame_pinned")?,
        last_frame: args.flag("has_last_frame")?,
    })
}

/// History ids are file names such as `20261028-121637-817`, or `last`: only letters, digits, `-`
/// and `_`, so no id reaches outside the history folder.
fn check_history_id(id: &str) -> Result<(), CliError> {
    let ok = !id.is_empty() && id.len() <= 100 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(CliError::usage(format!("invalid history id '{id}': use an id from list_history, or \"last\"")))
    }
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn choice(values: &[&str], description: &str) -> Value {
    json!({ "type": "string", "enum": values, "description": description })
}

fn boolean(description: &str) -> Value {
    json!({ "type": "boolean", "description": description })
}

fn number(min: f64, max: f64, description: &str) -> Value {
    json!({ "type": "number", "minimum": min, "maximum": max, "description": description })
}

fn integer(min: u64, max: u64, description: &str) -> Value {
    json!({ "type": "integer", "minimum": min, "maximum": max, "description": description })
}

/// All tools, in the order `tools/list` shows them.
fn tools() -> Vec<Tool> {
    let mode = |what: &str| choice(&["r2v", "f2v"], what);
    let thinking = || {
        choice(
            &["off", "low", "medium", "high"],
            "Bonsai's reasoning [default: the app's setting, off]. Off is tested best (valid prompts in 10-25 s); \
             thinking used thousands of tokens without answering on the 8 GB GPU",
        )
    };
    let temperature =
        |default: &str| number(0.0, 2.0, &format!("Sampling temperature [default: the app's setting, {default}]"));
    let max_tokens = |default: &str| {
        integer(256, 16_384, &format!("Most new tokens, reasoning included [default: the app's setting, {default}]"))
    };
    let auto_start = || {
        boolean(
            "Start Bonsai when it is not running (loading the model takes about a minute) [default: the app's \
             auto_start setting]",
        )
    };
    let save_history =
        || boolean("Save the prompt to the history, so fix_prompt can fix it [default: the app's setting, true]");
    let duration = |default: &str| {
        number(
            0.0,
            120.0,
            &format!(
                "Requested clip length in seconds; shot times and the FL2VA line are checked against its real \
                 length (6 s renders 6.58 s) [default: {default}]"
            ),
        )
    };
    let refs = |default: &str| {
        integer(
            1,
            request::MAX_R2V_REFS as u64,
            &format!("Number of pictures: r2v 1-9 (Ref 1..9), f2v 1-2 (first, last frame) [default: {default}]"),
        )
    };
    let pinned = |default: &str| boolean(&format!("r2v: Ref 1 is the exact first frame [default: {default}]"));
    let last_frame =
        |default: &str| boolean(&format!("f2v: a last frame (Picture 2) is used, FL2VA [default: {default}]"));
    let unknown = "unknown, its checks are skipped";
    vec![
        Tool {
            name: "bonsai_status",
            title: "Bonsai status",
            description:
                "Is the user's local Bonsai 27B (llama-server) ready? State (ready, loading, starting, stopped, \
                          failed), URL, model, context size, vision, the Bonsai-demo folder, auto-start and the GPU \
                          (memory, temperature, other programs such as ComfyUI). Changes nothing.",
            properties: json!({}),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
            run: tool_status,
        },
        Tool {
            name: "write_prompt",
            title: "Write an H3 prompt",
            description:
                "Write a MiniMax H3 video prompt with Bonsai from a request in plain words, clean it, check it \
                          against the official H3 format and save it to the history. Starts Bonsai when it is not \
                          running (loading takes about a minute) and leaves it running. Describe the pictures in \
                          words (\"Ref 1: ...\" for r2v, \"Picture 1: ...\" for f2v), the length (\"6 s\"), the cuts, \
                          the action, camera, style and dialogue in quotes. The check reads the length and picture \
                          facts from the request; pass duration_s / refs / first_frame_pinned / has_last_frame when \
                          the text is ambiguous (e.g. ages such as \"in her 20s\"). Returns the prompt (give it to \
                          the user unchanged) and the check report; fix problems with fix_prompt.",
            properties: json!({
                "mode": mode("Prompt kind: r2v (reference pictures Ref 1..9) or f2v (first frame, optional last frame)"),
                "request": string(
                    "The request in plain words, like the templates (list_templates): one line per picture, the \
                     length and cuts (\"6 s, one take: ...\"), the action, camera, style, dialogue in quotes"
                ),
                "duration_s": duration("read from the request"),
                "refs": refs("read from the request"),
                "first_frame_pinned": pinned("read from the request (\"Ref 1 is the first frame\"), else false"),
                "has_last_frame": last_frame("read from the request (\"Picture 2\", \"last frame\"), else false"),
                "images": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Picture files (absolute paths) for Bonsai's vision input, in Ref order; about \
                                    1,100 tokens and 18 s each - describing the pictures in words is usually enough"
                },
                "thinking": thinking(),
                "temperature": temperature("0.4"),
                "max_tokens": max_tokens("1500"),
                "auto_start": auto_start(),
                "save_history": save_history(),
            }),
            required: &["mode", "request"],
            read_only: false,
            destructive: false,
            idempotent: false,
            run: tool_write,
        },
        Tool {
            name: "fix_prompt",
            title: "Fix a saved H3 prompt",
            description: "Send a follow-up fix for a saved prompt (default: the newest) and get the new prompt with \
                          its check report; it is saved as a new history entry whose parent_id is the fixed one. Pass \
                          fix (what to change, e.g. \"Make the detailed_description longer (about 350 words).\") or \
                          from_lint: true (Bonsai fixes the errors and warnings the check finds). One round is \
                          usually enough; write a badly wrong prompt again instead. Starts Bonsai when needed.",
            properties: json!({
                "fix": string("What to change, in plain words (tested texts: see the CLI's `bh3 fixes`)"),
                "history_id": string("The entry to fix, from list_history or write_prompt's history_id [default: last]"),
                "from_lint": boolean("Send the check's errors and warnings as the fix (instead of fix)"),
                "thinking": thinking(),
                "temperature": temperature("0.4"),
                "max_tokens": max_tokens("1500"),
                "auto_start": auto_start(),
                "save_history": save_history(),
            }),
            required: &[],
            read_only: false,
            destructive: false,
            idempotent: false,
            run: tool_fix,
        },
        Tool {
            name: "lint_prompt",
            title: "Check an H3 prompt",
            description: "Check an H3 prompt against the official format without Bonsai: errors break the format, \
                          warnings hurt the video, info are hints (stable codes). Give the facts you know \
                          (duration_s, refs, first_frame_pinned, has_last_frame); unknown facts skip their checks. \
                          The mode is detected when it is missing.",
            properties: json!({
                "prompt": string("The H3 prompt text"),
                "mode": mode("Prompt kind [default: detected from the text]"),
                "duration_s": duration(unknown),
                "refs": refs(unknown),
                "first_frame_pinned": pinned(unknown),
                "has_last_frame": last_frame(unknown),
            }),
            required: &["prompt"],
            read_only: true,
            destructive: false,
            idempotent: true,
            run: tool_lint,
        },
        Tool {
            name: "clip_length",
            title: "H3 clip length",
            description: "What H3 renders for a requested length: frames and the real length (6 s renders 6.58 s; \
                          FL2VA lines and shot times use it), the suggested most cuts and the largest output size \
                          that rendered on the 8 GB laptop.",
            properties: json!({ "seconds": number(0.0, 120.0, "Requested length in seconds (more than 0)") }),
            required: &["seconds"],
            read_only: true,
            destructive: false,
            idempotent: true,
            run: tool_length,
        },
        Tool {
            name: "get_guide",
            title: "H3 prompt guide",
            description: "The short H3 prompt guide of a mode as the writer uses it, with the link to the official \
                          MiniMax guide.",
            properties: json!({ "mode": mode("r2v (reference pictures) or f2v (first frame)") }),
            required: &["mode"],
            read_only: true,
            destructive: false,
            idempotent: true,
            run: tool_guide,
        },
        Tool {
            name: "list_templates",
            title: "Request templates",
            description: "Request templates for write_prompt: plain-words requests with <...> parts to fill.",
            properties: json!({ "mode": mode("Only the templates of this mode [default: both]") }),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
            run: tool_templates,
        },
        Tool {
            name: "list_history",
            title: "Saved prompts",
            description: "Saved prompts, newest first: id, date, mode, title, check score and errors, parent_id (the \
                          entry a fix was made of) and starred.",
            properties: json!({
                "limit": integer(1, MAX_LIST_LIMIT, "Most entries to return [default: 20]"),
                "search": string("Only entries containing all these words (title, request, prompt, id)"),
                "starred": boolean("Only starred entries"),
            }),
            required: &[],
            read_only: true,
            destructive: false,
            idempotent: true,
            run: tool_history,
        },
        Tool {
            name: "get_history_entry",
            title: "Saved prompt",
            description: "One saved prompt by id, or \"last\" for the newest: request, prompt, reasoning, settings, \
                          timings, its check report, parent_id and the request form.",
            properties: json!({ "id": string("History id from list_history, or \"last\"") }),
            required: &["id"],
            read_only: true,
            destructive: false,
            idempotent: true,
            run: tool_entry,
        },
        Tool {
            name: "start_bonsai",
            title: "Start Bonsai",
            description: "Start Bonsai (llama-server of the Bonsai-demo folder) hidden in the background; it keeps \
                          running. write_prompt, fix_prompt and chat start it by themselves, so this only loads it \
                          ahead. A start is refused while other programs (ComfyUI?) hold the GPU memory; force: true \
                          starts anyway - only when the user agrees, Bonsai needs about 7.7 GB of the 8 GB GPU.",
            properties: json!({
                "wait": boolean("Return when the model is loaded, about a minute [default: true]"),
                "force": boolean("Start even while other programs hold the GPU memory [default: false]"),
                "timeout_s": integer(1, 86_400, "Seconds to wait with wait [default: the app's setting, 300]"),
            }),
            required: &[],
            read_only: false,
            destructive: false,
            idempotent: true,
            run: tool_start,
        },
        Tool {
            name: "stop_bonsai",
            title: "Stop Bonsai",
            description: "Stop Bonsai to free the GPU (about 7.7 GB), e.g. before the user renders in ComfyUI. It \
                          ends the llama-server on Bonsai's port, also one the user started: only call it when the \
                          user wants the GPU back.",
            properties: json!({}),
            required: &[],
            read_only: false,
            destructive: true,
            idempotent: true,
            run: tool_stop,
        },
        Tool {
            name: "chat",
            title: "Ask Bonsai",
            description: "Send one message to Bonsai and get its answer (nothing is saved). Starts Bonsai when needed.",
            properties: json!({
                "message": string("The message"),
                "system": string(
                    "System prompt: none (default), r2v or f2v (the H3 prompt writers' system prompts), or your own \
                     system prompt text"
                ),
                "thinking": thinking(),
                "temperature": temperature("0.7"),
                "max_tokens": max_tokens("2048"),
                "auto_start": auto_start(),
            }),
            required: &["message"],
            read_only: false,
            destructive: false,
            idempotent: false,
            run: tool_chat,
        },
    ]
}

// ---------------------------------------------------------------------------------------------
// Tools: functions
// ---------------------------------------------------------------------------------------------

fn tool_status(ctx: &mut Ctx, _args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let status = status_report(ctx);
    let auto_start = ctx.config().auto_start;
    let props = status.props.as_ref();
    let no_server = status.llama_server_pids.is_empty();
    let message = match status.state {
        "stopped" if no_server && auto_start => {
            Some("Nothing answers there; write_prompt, fix_prompt and chat start Bonsai when needed.".to_string())
        }
        "stopped" if no_server => Some(
            "Nothing answers there, and auto-start is off: call start_bonsai first (or pass auto_start: true)."
                .to_string(),
        ),
        _ => status.message.as_deref().map(tool_text),
    };
    let data = json!({
        "state": status.state,
        "state_label": status.state_label,
        "ready": status.ready,
        "url": status.url,
        "message": message,
        "model": props.map(|p| if p.model_name.is_empty() { &p.model_path } else { &p.model_name }),
        "n_ctx": props.map(|p| p.n_ctx),
        "slots": props.map(|p| p.total_slots),
        "vision": props.map(|p| p.vision),
        "llama_server_pids": status.llama_server_pids,
        "install": status.install.as_ref().map(|i| json!({
            "dir": i.root,
            "usable": i.usable,
            "problems": i.problems,
            "backend": i.backend,
            "model": i.model,
        })),
        "auto_start": auto_start,
        "gpu": status.gpu.as_ref().map(|g| json!({
            "name": g.info.name,
            "used_mb": g.info.mem_used_mb,
            "total_mb": g.info.mem_total_mb,
            "temperature_c": g.info.temp_c,
            "load_pct": g.info.util_pct,
            "other_processes": g.other_processes,
            // Why a start would be refused; only while no llama-server holds the memory itself.
            "busy": g.busy.as_ref().filter(|_| no_server),
        })),
    });

    let mut parts = vec![format!("Bonsai: {} at {}.", status.state_label, status.url)];
    if let Some(p) = props {
        let name = if p.model_name.is_empty() { &p.model_path } else { &p.model_name };
        parts.push(format!(
            "Model {name}, context {}, {}, vision {}.",
            p.n_ctx,
            plural(p.total_slots as usize, "slot"),
            if p.vision { "on" } else { "off" }
        ));
    }
    parts.extend(message);
    parts.push(match &status.install {
        Some(i) if i.usable => format!("Bonsai folder: {} (usable).", i.root.display()),
        Some(i) => format!("Bonsai folder: {} (not usable: {}).", i.root.display(), i.problems.join("; ")),
        None => "No Bonsai-demo folder is set or found (the user sets it with `bh3 install set <DIR>`).".to_string(),
    });
    match &status.gpu {
        Some(g) => {
            let mut line = format!(
                "GPU: {}, {} of {} used, {} °C",
                g.info.name,
                gpu::fmt_mb(g.info.mem_used_mb),
                gpu::fmt_mb(g.info.mem_total_mb),
                g.info.temp_c
            );
            if !g.other_processes.is_empty() {
                let names: Vec<String> = g.other_processes.iter().map(gpu::describe_process).collect();
                line.push_str(&format!("; other programs on it: {}", names.join(", ")));
            }
            line.push('.');
            parts.push(line);
            if let Some(busy) = g.busy.as_ref().filter(|_| no_server) {
                parts.push(format!("A start would be refused now: {busy}"));
            }
        }
        None => parts.push("No NVIDIA GPU status (nvidia-smi is missing?).".to_string()),
    }
    Ok(ToolOutput::ok(parts.join(" "), data))
}

fn tool_write(ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let mode = mode_arg(args, "mode")?.unwrap_or_default();
    let request = normalize_text(args.text("request")?.unwrap_or_default());
    let context = context_args(args)?;
    let gen = gen_args(args)?;
    let images = args.texts("images")?;
    if images.len() > request::max_refs(mode) {
        return Err(CliError::usage(format!(
            "write_prompt: {mode} prompts use at most {} pictures",
            request::max_refs(mode)
        )));
    }
    let save_history = args.flag("save_history")?;
    ctx.auto_start = args.flag("auto_start")?;
    let cfg = ctx.config();
    let mut job = text_job(mode, &request, gen.apply(cfg.writer_params.clone()))?;
    job.lint_ctx = context.apply(job.lint_ctx.clone())?;
    job.save_history = save_history.unwrap_or(cfg.save_history);
    for path in images {
        job.images.push(ImageAttachment::from_file(Path::new(path)).map_err(CliError::from)?);
    }
    let done = run_job(ctx, &job, &RunArgs::default(), Vec::new())?;
    Ok(prompt_output(&job, &done))
}

fn tool_fix(ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let fix = args.text("fix")?.map(normalize_text);
    let from_lint = args.flag("from_lint")?.unwrap_or(false);
    if fix.is_some() == from_lint {
        return Err(CliError::usage("fix_prompt: pass either fix (what to change) or from_lint: true"));
    }
    if fix.as_deref().is_some_and(|f| f.trim().is_empty()) {
        return Err(CliError::usage("fix_prompt: 'fix' is empty: say what to change"));
    }
    let id = args.text("history_id")?.map_or("last", str::trim);
    check_history_id(id)?;
    let fix_args = FixArgs {
        id: Some(id.to_string()),
        last: false,
        fix,
        from_lint,
        context: ContextArgs::default(),
        gen: gen_args(args)?,
        prompt: PromptArgs::default(),
        run: RunArgs::default(),
    };
    let save_history = args.flag("save_history")?;
    ctx.auto_start = args.flag("auto_start")?;
    let (mut job, warnings) = fix_job(ctx, &fix_args)?;
    if let Some(save) = save_history {
        job.save_history = save;
    }
    let done = run_job(ctx, &job, &fix_args.run, warnings)?;
    Ok(prompt_output(&job, &done))
}

/// structuredContent of `write_prompt` / `fix_prompt`.
#[derive(Debug, Serialize)]
struct PromptJson<'a> {
    prompt: &'a str,
    mode: Mode,
    lint: LintJson<'a>,
    lint_context: &'a LintContext,
    history_id: Option<&'a str>,
    parent_id: Option<&'a str>,
    started_server: bool,
    finish_reason: &'a str,
    timings: Option<&'a Timings>,
    warnings: Vec<String>,
}

/// The prompt first, then a blank line and the check summary, the history id and the warnings.
fn prompt_output(job: &WriteJob, done: &JobRun) -> ToolOutput {
    let outcome = &done.outcome;
    let warnings: Vec<String> = done.warnings.iter().map(|w| tool_text(w)).collect();
    let mut lines = vec![
        outcome.prompt.clone(),
        String::new(),
        format!("Check: {} ({})", score_words(&outcome.lint), context_words(&done.lint_context)),
    ];
    lines.extend(
        outcome.lint.issues.iter().filter(|i| i.severity != Severity::Info).map(|i| format!("  {}", issue_line(i))),
    );
    if let Some(id) = &outcome.history_id {
        lines.push(format!("History id: {id}"));
    }
    lines.extend(warnings.iter().map(|w| format!("Warning: {w}")));
    let data = value_of(&PromptJson {
        prompt: &outcome.prompt,
        mode: outcome.lint.mode,
        lint: LintJson::new(&outcome.lint),
        lint_context: &done.lint_context,
        history_id: outcome.history_id.as_deref(),
        parent_id: job.followup.as_ref().and_then(|f| f.parent_id.as_deref()),
        started_server: done.session.started,
        finish_reason: &outcome.result.finish_reason,
        timings: outcome.result.timings.as_ref(),
        warnings,
    });
    ToolOutput::ok(lines.join("\n"), data)
}

fn tool_lint(_ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let prompt = normalize_text(args.text("prompt")?.unwrap_or_default());
    if prompt.trim().is_empty() {
        return Err(CliError::usage("lint_prompt: 'prompt' is empty"));
    }
    let mut note = None;
    let mode = match mode_arg(args, "mode")? {
        Some(mode) => mode,
        None => lint::detect_mode(&prompt).unwrap_or_else(|| {
            note = Some("the text is neither an r2v nor an f2v prompt; it was checked as r2v (pass mode)");
            Mode::R2v
        }),
    };
    let lint_ctx = context_args(args)?.apply(LintContext { mode, ..LintContext::default() })?;
    let report = lint::lint(&prompt, &lint_ctx);
    let mut data = value_of(&LintCmdJson { lint: LintJson::new(&report), context: &lint_ctx });
    let mut text = lint_text(&report, &lint_ctx);
    if let Some(note) = note {
        data["note"] = json!(note);
        text = format!("note: {note}\n{text}");
    }
    Ok(ToolOutput::ok(text, data))
}

fn tool_length(_ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let seconds = args.seconds("seconds")?.unwrap_or_default();
    let row = LengthRow::of(seconds);
    let text = format!(
        "{} s: H3 renders {} frames = {} s real (FL2VA lines and shot times use {}). At most {} suggested; the \
         largest size that rendered on the 8 GB laptop is {} MP.",
        fmt_num(seconds),
        row.frames,
        row.real,
        row.real,
        plural(row.max_cuts as usize, "cut"),
        row.max_megapixels
    );
    let data = json!({
        "seconds": row.seconds,
        "frames": row.frames,
        "real_seconds": row.real_s,
        "real_text": row.real,
        "suggested_max_cuts": row.max_cuts,
        "laptop_max_megapixels": row.max_megapixels,
    });
    Ok(ToolOutput::ok(text, data))
}

fn tool_guide(_ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let mode = mode_arg(args, "mode")?.unwrap_or_default();
    let (item, url) = match mode {
        Mode::R2v => (LibraryItem::GuideR2v, crate::GUIDE_REF_URL),
        Mode::F2v => (LibraryItem::GuideF2v, crate::GUIDE_BASE_URL),
    };
    let library = Library::load();
    let guide = library.text(item);
    let data = json!({ "mode": mode, "text": guide, "official_guide": url, "overridden": library.is_overridden(item) });
    Ok(ToolOutput::ok(format!("{}\n\nOfficial MiniMax guide: {url}", guide.trim_end()), data))
}

fn tool_templates(_ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let wanted = mode_arg(args, "mode")?;
    let templates = Library::load().templates();
    let shown: Vec<TemplateJson<'_>> = templates
        .iter()
        .enumerate()
        .filter(|(_, t)| wanted.is_none_or(|mode| t.mode == mode))
        .map(|(i, template)| TemplateJson { number: i + 1, template })
        .collect();
    let blocks: Vec<String> = shown
        .iter()
        .map(|t| {
            let template = t.template;
            format!(
                "{}. {} ({}, id {})\n{}",
                t.number,
                template.title,
                template.mode,
                template.id,
                template.text.trim_end()
            )
        })
        .collect();
    let text = if blocks.is_empty() { "No templates.".to_string() } else { blocks.join("\n\n") };
    Ok(ToolOutput::ok(text, json!({ "templates": value_of(&shown) })))
}

fn tool_history(_ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let limit = args.integer("limit", 1, MAX_LIST_LIMIT)?.map_or(DEFAULT_HISTORY_LIMIT, |n| n as usize);
    let query = args.text("search")?.unwrap_or_default();
    let starred = args.flag("starred")?.unwrap_or(false);
    let entries: Vec<HistoryEntry> =
        history::list().into_iter().filter(|e| !starred || e.starred).filter(|e| e.matches(query)).collect();
    let shown = &entries[..entries.len().min(limit)];
    let mut lines: Vec<String> = shown
        .iter()
        .map(|e| {
            let score = e.lint_score.map_or_else(|| "-".to_string(), |s| s.to_string());
            let star = if e.starred { "* " } else { "" };
            let fix = e.parent_id.as_ref().map(|p| format!(" (fix of {p})")).unwrap_or_default();
            format!(
                "{}  {}  {}  score {score}, {}  {star}{}{fix}",
                e.id,
                e.short_date(),
                e.mode,
                plural(e.lint_errors, "error"),
                e.title
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push("No saved prompts.".to_string());
    } else if shown.len() < entries.len() {
        lines.push(format!("({} of {} shown)", shown.len(), entries.len()));
    }
    let data = json!({ "entries": value_of(&shown.iter().map(HistorySummary::of).collect::<Vec<_>>()), "total": entries.len() });
    Ok(ToolOutput::ok(lines.join("\n"), data))
}

fn tool_entry(_ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let id = args.text("id")?.unwrap_or_default().trim();
    check_history_id(id)?;
    let entry = find_entry(id)?;
    let lint_ctx = writer::entry_lint_context(&entry);
    let report = lint::lint(&entry.output, &lint_ctx);
    let mut data = value_of(&entry);
    data["lint"] = value_of(&LintJson::new(&report));
    let mut lines = vec![
        entry_text(&entry),
        String::new(),
        format!("Check: {} ({})", score_words(&report), context_words(&lint_ctx)),
    ];
    lines.extend(report.issues.iter().filter(|i| i.severity != Severity::Info).map(|i| format!("  {}", issue_line(i))));
    Ok(ToolOutput::ok(lines.join("\n"), data))
}

fn tool_start(ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let start = StartArgs {
        wait: args.flag("wait")?.unwrap_or(true),
        timeout: args.integer("timeout_s", 1, 86_400)?,
        force: args.flag("force")?.unwrap_or(false),
    };
    let action = start_server(ctx, &start, false)?;
    Ok(ToolOutput::ok(tool_text(&action.message), value_of(&action)))
}

fn tool_stop(ctx: &mut Ctx, _args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let action = stop_server(ctx)?;
    let text = if action.stopped {
        "Bonsai stopped: the GPU memory is free for ComfyUI. \
         write_prompt starts Bonsai again when it is needed."
    } else {
        "Bonsai is not running: nothing was stopped."
    };
    Ok(ToolOutput::ok(text, value_of(&action)))
}

/// structuredContent of `chat`.
#[derive(Debug, Serialize)]
struct ChatToolJson<'a> {
    content: &'a str,
    reasoning: &'a str,
    finish_reason: &'a str,
    timings: Option<&'a Timings>,
    started_server: bool,
    warnings: Vec<String>,
}

fn tool_chat(ctx: &mut Ctx, args: &ToolArgs<'_>) -> Result<ToolOutput, CliError> {
    let message = normalize_text(args.text("message")?.unwrap_or_default());
    if message.trim().is_empty() {
        return Err(CliError::usage("chat: 'message' is empty"));
    }
    let system = args.text("system")?.unwrap_or_default().trim();
    let gen = gen_args(args)?;
    ctx.auto_start = args.flag("auto_start")?;
    let mut messages = Vec::new();
    match system.to_ascii_lowercase().as_str() {
        "" | "none" | "off" | "no" => {}
        "r2v" => messages.push(ChatMessage::system(Library::load().system_prompt(Mode::R2v))),
        "f2v" => messages.push(ChatMessage::system(Library::load().system_prompt(Mode::F2v))),
        _ => messages.push(ChatMessage::system(normalize_text(system))),
    }
    messages.push(ChatMessage::user(message.trim()));
    let params = gen.apply(ctx.config().chat_params.clone());
    let done = run_chat(ctx, &messages, &params, &RunArgs::default())?;
    let result = &done.result;
    let warnings: Vec<String> = done.warnings.iter().map(|w| tool_text(w)).collect();
    let mut text = result.content.trim().to_string();
    for warning in &warnings {
        text.push_str(&format!("\n\nWarning: {warning}"));
    }
    let data = value_of(&ChatToolJson {
        content: result.content.trim(),
        reasoning: result.reasoning.trim(),
        finish_reason: &result.finish_reason,
        timings: result.timings.as_ref(),
        started_server: done.session.started,
        warnings,
    });
    Ok(ToolOutput::ok(text, data))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn offline_ctx() -> Ctx {
        Ctx::new(&GlobalArgs { url: Some("http://127.0.0.1:9".to_string()), ..GlobalArgs::default() })
    }

    fn reply(server: &Server<'_>, text: &str) -> Option<Value> {
        server.handle_text(text).map(|reply| {
            assert!(!reply.contains('\n'), "one line per message: {reply}");
            serde_json::from_str(&reply).unwrap()
        })
    }

    #[test]
    fn protocol_versions_are_negotiated() {
        assert_eq!(negotiate(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate(Some("2099-01-01")), LATEST_PROTOCOL);
        assert_eq!(negotiate(None), LATEST_PROTOCOL);
        let init = initialize_result(Some(&json!({ "protocolVersion": "2025-03-26" })));
        assert_eq!(init["protocolVersion"], "2025-03-26");
        assert_eq!(init["serverInfo"]["name"], "bonsai-h3-writer");
        assert_eq!(init["capabilities"]["tools"]["listChanged"], false);
    }

    #[test]
    fn requests_notifications_and_bad_messages() {
        let ctx = offline_ctx();
        let server = Server::new(&ctx);
        let ping = reply(&server, r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#);
        assert_eq!(ping, Some(json!({ "jsonrpc": "2.0", "id": 7, "result": {} })));
        assert_eq!(reply(&server, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
        assert_eq!(reply(&server, r#"{"jsonrpc":"2.0","method":"ping"}"#), None, "no id: a notification");
        assert_eq!(reply(&server, r#"{"jsonrpc":"2.0","id":"a","result":{}}"#), None, "a response");
        assert_eq!(reply(&server, " \r"), None, "a blank line");
        let parse = reply(&server, "{not json").unwrap();
        assert_eq!((&parse["id"], &parse["error"]["code"]), (&Value::Null, &json!(PARSE_ERROR)));
        for bad in [
            r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#,
            r#"{"id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            "42",
            "[]",
        ] {
            assert_eq!(reply(&server, bad).unwrap()["error"]["code"], INVALID_REQUEST, "{bad}");
        }
        let unknown = reply(&server, r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#).unwrap();
        assert_eq!(unknown["error"]["code"], METHOD_NOT_FOUND);
        let batch = reply(
            &server,
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},
                {"jsonrpc":"2.0","id":"b","method":"ping"}]"#
                .replace('\n', " ")
                .as_str(),
        )
        .unwrap();
        let ids: Vec<&Value> = batch.as_array().unwrap().iter().map(|r| &r["id"]).collect();
        assert_eq!(ids, [&json!(1), &json!("b")]);
        assert_eq!(reply(&server, r#"[{"jsonrpc":"2.0","method":"notifications/initialized"}]"#), None);
        let list = reply(&server, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"cursor":"x"}}"#);
        assert_eq!(list.unwrap()["result"]["tools"].as_array().map(Vec::len), Some(12));
    }

    #[test]
    fn tools_are_listed_with_their_schemas() {
        let tools = tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 12, "{names:?}");
        for tool in &tools {
            let definition = tool.definition();
            let schema = &definition["inputSchema"];
            assert_eq!(schema["type"], "object", "{}", tool.name);
            assert_eq!(schema["additionalProperties"], false);
            let properties = schema["properties"].as_object().unwrap();
            for key in tool.required {
                assert!(properties.contains_key(*key), "{}: {key}", tool.name);
            }
            for (key, property) in properties {
                assert!(property["description"].as_str().is_some_and(|d| !d.is_empty()), "{}: {key}", tool.name);
            }
            assert_eq!(definition["annotations"]["openWorldHint"], false);
            assert_eq!(definition["annotations"]["destructiveHint"], tool.name == "stop_bonsai");
        }
    }

    #[test]
    fn tool_arguments_are_checked() {
        let tools = tools();
        let lint = tools.iter().find(|t| t.name == "lint_prompt").unwrap();
        let map = |value: Value| value.as_object().unwrap().clone();
        let unknown = map(json!({ "prompt": "x", "bogus": 1 }));
        assert!(ToolArgs::check(lint, &unknown).err().unwrap().message.contains("unknown argument 'bogus'"));
        let missing = map(json!({ "mode": "r2v", "prompt": null }));
        assert!(ToolArgs::check(lint, &missing).err().unwrap().message.contains("'prompt' is required"));

        let lenient = map(json!({
            "prompt": "x", "duration_s": "6.5", "refs": 2.0, "first_frame_pinned": "yes", "has_last_frame": null,
            "mode": "FL2VA"
        }));
        let args = ToolArgs::check(lint, &lenient).unwrap();
        assert_eq!(args.seconds("duration_s").unwrap(), Some(6.5));
        assert_eq!(args.integer("refs", 1, 9).unwrap(), Some(2));
        assert_eq!(args.flag("first_frame_pinned").unwrap(), Some(true));
        assert_eq!(args.flag("has_last_frame").unwrap(), None);
        assert_eq!(mode_arg(&args, "mode").unwrap(), Some(Mode::F2v));

        let wrong =
            map(json!({ "prompt": 5, "duration_s": 0, "refs": 2.5, "first_frame_pinned": "maybe", "mode": "t2v" }));
        let args = ToolArgs::check(lint, &wrong).unwrap();
        assert!(args.text("prompt").is_err());
        assert!(args.seconds("duration_s").is_err());
        assert!(args.integer("refs", 1, 9).is_err());
        assert!(args.flag("first_frame_pinned").is_err());
        let err = mode_arg(&args, "mode").unwrap_err();
        assert_eq!((err.code, err.message.as_str()), (EXIT_USAGE, "lint_prompt: 'mode' must be \"r2v\" or \"f2v\""));
    }

    #[test]
    fn cancellations_before_and_during_a_call() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        let mut calls = Calls::new(&FLAG);
        calls.cancel(&json!(1));
        assert!(!calls.begin(&json!(1)), "cancelled before it started");
        assert!(calls.begin(&json!(2)));
        calls.cancel(&json!("other"));
        assert!(!FLAG.load(Ordering::SeqCst), "another request");
        calls.cancel(&json!(2));
        assert!(FLAG.load(Ordering::SeqCst), "the running call");
        calls.end();
        assert!(calls.begin(&json!(3)));
        assert!(!FLAG.load(Ordering::SeqCst), "a new call clears the flag");
        for i in 0..100 {
            calls.cancel(&json!(format!("x{i}")));
        }
        assert_eq!(calls.early.len(), MAX_EARLY_CANCELS);

        let batch = json!([
            { "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 4, "reason": "x" } },
            { "jsonrpc": "2.0", "id": 5, "method": "ping" }
        ]);
        assert_eq!(cancelled_ids(&batch), [&json!(4)]);
    }

    #[test]
    fn offline_tools_answer_through_the_protocol() {
        let ctx = offline_ctx();
        let server = Server::new(&ctx);
        let call = |name: &str, arguments: Value| {
            let request = json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments }
            });
            reply(&server, &request.to_string()).unwrap()
        };
        let good = include_str!("../../tests/fixtures/good_r2v_e1.txt");
        let lint =
            call("lint_prompt", json!({ "prompt": good, "duration_s": 3, "refs": 1, "first_frame_pinned": false }));
        let result = &lint["result"];
        assert_eq!((&result["isError"], &result["structuredContent"]["ok"]), (&json!(false), &json!(true)), "{lint}");
        assert_eq!(result["structuredContent"]["context"]["duration_s"], 3.0);
        assert!(result["content"][0]["text"].as_str().unwrap().contains("OK: no errors."));

        let length = call("clip_length", json!({ "seconds": 6 }));
        assert_eq!(length["result"]["structuredContent"]["real_text"], "6.58");
        assert_eq!(length["result"]["structuredContent"]["frames"], 158);

        // A bad value is a failed tool call; an unknown tool or arguments that are no object are
        // protocol errors.
        let negative = call("clip_length", json!({ "seconds": -1 }));
        assert_eq!(negative["result"]["isError"], true);
        assert_eq!(negative["result"]["structuredContent"]["kind"], "usage");
        assert_eq!(call("no_such_tool", json!({}))["error"]["code"], INVALID_PARAMS);
        assert_eq!(call("clip_length", json!([6]))["error"]["code"], INVALID_PARAMS);
        // Refused before any file is read.
        let outside = call("get_history_entry", json!({ "id": "../x" }));
        assert_eq!(outside["result"]["isError"], true);
        let both = call("fix_prompt", json!({ "fix": "Make it longer.", "from_lint": true }));
        assert_eq!(both["result"]["isError"], true);
    }

    #[test]
    fn cli_hints_become_tool_names() {
        assert_eq!(
            tool_text("Bonsai is not running at http://x (auto_start is off in the settings). Start it with `bh3 start --wait`."),
            "Bonsai is not running at http://x (auto_start is off in the settings). Start it with start_bonsai."
        );
        assert!(tool_text("thinking used all 1500 tokens: use --thinking off or raise --max-tokens")
            .ends_with("use thinking \"off\" or raise max_tokens"));
        assert_eq!(
            tool_text("cancelled (Ctrl+C): the request to Bonsai was aborted"),
            "cancelled: the request to Bonsai was aborted"
        );
        assert_eq!(
            tool_text("--refs 3: f2v prompts use at most 2 pictures"),
            "refs 3: f2v prompts use at most 2 pictures"
        );
        assert!(tool_text("Not starting Bonsai: x.\nClose the other program, or start Bonsai anyway with `bh3 start --force --wait` and run this command again.")
            .ends_with("with the start_bonsai tool (force: true) and call this tool again."));
    }

    #[test]
    fn history_ids_stay_inside_the_history_folder() {
        for ok in ["last", "20261028-121637-817", "20261028-121637-817-2"] {
            assert!(check_history_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "../x", "..\\x", "a/b", "C:x", ".hidden", "a b"] {
            assert!(check_history_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn values_keep_f32_numbers_short() {
        let ctx = LintContext { mode: Mode::R2v, duration_s: Some(6.58), ..LintContext::default() };
        assert_eq!(value_of(&ctx)["duration_s"].to_string(), "6.58");
        assert_eq!(value_of(&GenParams::prompt_writer())["temperature"].to_string(), "0.4");
    }
}
