//! HTTP client for Bonsai's llama-server (OpenAI-compatible API + llama.cpp extras).
//!
//! Endpoints used (verified against the user's server, build b10787):
//! - `GET /health` -> 200 `{"status":"ok"}` when ready, 503 while the model loads.
//! - `GET /props` -> `model_path`, `model_ftype`, `total_slots`, `build_info`,
//!   `modalities.vision`, `default_generation_settings.n_ctx`.
//! - `GET /slots` -> array of slots with `is_processing`, `n_ctx`.
//! - `POST /tokenize` `{"content": text}` -> `{"tokens": [..]}`.
//! - `POST /v1/chat/completions` (see [`crate::gen::build_chat_body`]). Streaming answers are SSE
//!   lines `data: {...}`; each chunk has `choices[0].delta.reasoning_content` (thinking) and/or
//!   `choices[0].delta.content` (answer); `choices[0].finish_reason` on the last choice chunk;
//!   a final chunk with empty `choices` carries `usage` (`prompt_tokens`, `completion_tokens`,
//!   `prompt_tokens_details.cached_tokens`) and `timings` (`prompt_n`, `cache_n`, `prompt_ms`,
//!   `prompt_per_second`, `predicted_n`, `predicted_ms`, `predicted_per_second`); then `data: [DONE]`.
//!   Non-streaming answers carry the same fields in `choices[0].message`, `usage`, `timings`.
//! - Errors: a non-2xx status with `{"error":{"code":..,"message":..,"type":..}}`, reported as
//!   `Bonsai error <code>: <message>`. A streaming request that fails before its first token gets
//!   such a plain JSON answer too; a later failure arrives as an `error` object in the stream.
//!
//! Transport:
//! - `/health`, `/props`, `/slots`, `/tokenize` and the non-streaming chat use ureq: 3 s connect
//!   timeout, short per-request timeouts on the info calls, none on the chat, and no connection
//!   pool (llama-server closes idle keep-alive connections after 5 s, and ureq does not retry a
//!   POST that runs into such a stale connection). Like the streaming chat, IPv4 addresses are
//!   tried first (`localhost` may resolve to `::1` first, and a refused local connect costs ~2 s).
//! - The streaming chat speaks HTTP/1.1 over a raw [`TcpStream`]: llama-server sends nothing at
//!   all, not even the response headers, until prompt processing is done (30+ s for a long system
//!   prompt on the laptop), and a cancel must still take effect at once. The socket is
//!   non-blocking and polled every 10 ms, checking the cancel flag in between. (A read timeout is
//!   not used: on Windows a timed-out blocking receive leaves the TCP connection in an
//!   indeterminate state.) On cancel the socket is shut down; llama-server notices the closed
//!   connection and aborts the generation.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gen::{build_chat_body, ChatMessage, GenParams};

/// Server address used when the configured URL is empty.
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";
/// Connect timeout of every request.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Whole-request timeout of `GET /health`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// Whole-request timeout of `/props`, `/slots` and `/tokenize`.
const INFO_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause before polling the non-blocking stream socket again when no data is ready.
/// Bounds the cancel latency and the extra delay of a streamed piece.
const STREAM_IDLE_PAUSE: Duration = Duration::from_millis(10);
/// Pause before retrying a send when the socket's send buffer is full.
const SEND_RETRY_PAUSE: Duration = Duration::from_millis(1);
/// Give up sending a request when llama-server accepts no bytes for this long.
const SEND_STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// After `data: [DONE]`, wait at most this long for the end of the HTTP body before closing.
const DONE_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
/// Size of the socket read buffer.
const READ_BUF_BYTES: usize = 16 * 1024;
/// Upper bound for the response headers.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Upper bound for an error body kept for the error message.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// Upper bound for one SSE line or for a non-streamed JSON answer on the stream connection.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Longest error text taken over from a non-JSON error body.
const MAX_ERROR_CHARS: usize = 300;

/// Result of `GET /health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Health {
    /// 200: model loaded, ready for requests.
    Ready,
    /// 503: server process up, model still loading.
    Loading,
    /// No answer (connection refused / timeout) or another error.
    Down(String),
}

/// Model and server facts from `GET /props`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerProps {
    /// Full path of the loaded GGUF.
    pub model_path: String,
    /// File name of the loaded GGUF (derived from `model_path`).
    pub model_name: String,
    /// e.g. `PTQ1_0 - 1.75 bpw ternary (group 128)`.
    pub model_ftype: String,
    /// Context size per slot.
    pub n_ctx: u32,
    /// Number of parallel slots (`--parallel`).
    pub total_slots: u32,
    /// True when an mmproj is loaded (image input works).
    pub vision: bool,
    /// llama.cpp build, e.g. `b10787-73bfe6cc4`.
    pub build_info: String,
}

/// What `GET /slots` says about requests (see [`BonsaiClient::slots_activity`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotsActivity {
    /// A slot is processing a request (prompt or generation) right now.
    pub processing: bool,
    /// Per slot `(id, id_task, n_decoded)` (-1 when missing): changes when a request ran.
    pub tasks: Vec<(i64, i64, i64)>,
}

impl SlotsActivity {
    /// Busy now, or a request ran since `before` was taken (task ids or decode counts changed).
    pub fn active_since(&self, before: Option<&SlotsActivity>) -> bool {
        self.processing || before.is_some_and(|b| b.tasks != self.tasks)
    }
}

/// Speed and cache numbers llama-server reports per request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timings {
    /// New prompt tokens processed.
    pub prompt_n: u32,
    /// Prompt tokens reused from the cache.
    pub cache_n: u32,
    /// Time spent processing the new prompt tokens.
    pub prompt_ms: f64,
    /// Prompt processing speed (tokens per second).
    pub prompt_per_second: f64,
    /// Generated tokens (reasoning + answer).
    pub predicted_n: u32,
    /// Time spent generating.
    pub predicted_ms: f64,
    /// Generation speed (tokens per second).
    pub predicted_per_second: f64,
}

/// Token usage of one request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens (cached + new).
    pub prompt_tokens: u32,
    /// Generated tokens (reasoning + answer).
    pub completion_tokens: u32,
    /// `prompt_tokens + completion_tokens`.
    pub total_tokens: u32,
    /// Prompt tokens reused from the cache (`prompt_tokens_details.cached_tokens`).
    pub cached_tokens: u32,
}

/// The finished answer of one chat request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatResult {
    /// The answer text (`content`).
    pub content: String,
    /// The thinking text (`reasoning_content`), empty with thinking off.
    pub reasoning: String,
    /// `stop`, `length` (hit max_tokens), or `cancelled`.
    pub finish_reason: String,
    /// Speed and cache numbers (absent when the request was cancelled early).
    pub timings: Option<Timings>,
    /// Token counts (absent when the request was cancelled early).
    pub usage: Option<Usage>,
    /// Wall-clock time of the request.
    pub elapsed_ms: u64,
    /// True when the caller cancelled a stream.
    pub cancelled: bool,
}

/// One streamed piece of text.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A piece of the thinking text (`reasoning_content`).
    Reasoning(String),
    /// A piece of the answer (`content`).
    Content(String),
}

/// What one SSE `data:` line contained (public for unit tests).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SseChunk {
    /// `choices[0].delta.reasoning_content`.
    pub reasoning: Option<String>,
    /// `choices[0].delta.content` (`None` for `null` or a missing field).
    pub content: Option<String>,
    /// `finish_reason` of the last choice that has one.
    pub finish_reason: Option<String>,
    /// `usage` of the final chunk.
    pub usage: Option<Usage>,
    /// `timings` of the final chunk.
    pub timings: Option<Timings>,
    /// The `data: [DONE]` terminator.
    pub done: bool,
    /// An `error` object in the stream (message).
    pub error: Option<String>,
}

/// Blocking client (ureq). Cheap to clone; safe to use from several threads.
#[derive(Clone)]
pub struct BonsaiClient {
    base: String,
    agent: ureq::Agent,
}

impl BonsaiClient {
    /// `base_url` like `http://127.0.0.1:8080` (trailing slash allowed).
    /// Connect timeout ~3 s; no overall timeout on generation requests (they can take minutes).
    /// An empty URL means `http://127.0.0.1:8080`; a URL without a scheme gets `http://`.
    pub fn new(base_url: &str) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .try_proxy_from_env(false)
            .user_agent(&user_agent())
            .resolver(resolve_ipv4_first)
            .build();
        Self { base: normalize_base_url(base_url), agent }
    }

    /// Base URL without trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// `GET /health` with a ~2 s timeout.
    ///
    /// When nothing listens, Windows needs about 2 s to refuse a localhost connection, so a
    /// stopped server shows up as `Down` after about that long.
    pub fn health(&self) -> Health {
        match self.agent.get(&self.url("/health")).timeout(HEALTH_TIMEOUT).call() {
            Ok(_) => Health::Ready,
            Err(ureq::Error::Status(503, _)) => Health::Loading,
            Err(ureq::Error::Status(code, response)) => {
                Health::Down(status_error_from_response(code, response).to_string())
            }
            Err(ureq::Error::Transport(transport)) => Health::Down(transport_message(&transport)),
        }
    }

    /// `GET /props`.
    pub fn props(&self) -> anyhow::Result<ServerProps> {
        let value = self.get_json("/props", INFO_TIMEOUT)?;
        Ok(props_from_json(&value))
    }

    /// `GET /slots`: true when any slot `is_processing`.
    ///
    /// llama-server answers this between two compute batches, so it can take a few seconds
    /// while a long prompt is processed (10 s timeout).
    pub fn slots_busy(&self) -> anyhow::Result<bool> {
        let value = self.get_json("/slots", INFO_TIMEOUT)?;
        let slots =
            value.as_array().context("Bonsai's /slots answer is not a list (is the slots endpoint disabled?)")?;
        Ok(slots.iter().any(slot_is_processing))
    }

    /// `GET /slots` with a short `timeout`, for the idle auto-stop (which must not stop a server
    /// that other programs use): whether a slot is processing and which tasks the slots ran last.
    ///
    /// `Ok(None)` when the server has no slots endpoint (`--no-slots` answers 501) or another
    /// error status; `Err` when it does not answer in time (it may be busy with a long prompt).
    pub fn slots_activity(&self, timeout: Duration) -> anyhow::Result<Option<SlotsActivity>> {
        match self.agent.get(&self.url("/slots")).timeout(timeout).call() {
            Ok(response) => {
                let text = response.into_string().context("reading Bonsai's answer to /slots")?;
                let value: Value =
                    serde_json::from_str(&text).context("Bonsai's answer to /slots is not valid JSON")?;
                Ok(slots_activity_from_json(&value))
            }
            Err(ureq::Error::Status(..)) => Ok(None),
            Err(ureq::Error::Transport(transport)) => {
                Err(anyhow!("cannot reach Bonsai at {}: {}", self.base, transport_message(&transport)))
            }
        }
    }

    /// `POST /tokenize`: number of tokens in `text`.
    pub fn tokenize_count(&self, text: &str) -> anyhow::Result<usize> {
        let value = self.post_json("/tokenize", &json!({ "content": text }), Some(INFO_TIMEOUT))?;
        let tokens =
            value.get("tokens").and_then(Value::as_array).context("Bonsai's /tokenize answer has no token list")?;
        Ok(tokens.len())
    }

    /// Non-streaming chat completion.
    pub fn chat(&self, messages: &[ChatMessage], params: &GenParams) -> anyhow::Result<ChatResult> {
        let started = Instant::now();
        let body = build_chat_body(messages, params, false);
        let value = self.post_json("/v1/chat/completions", &body, None)?;
        let mut result = completion_from_json(&value)?;
        result.elapsed_ms = elapsed_ms(started);
        Ok(result)
    }

    /// Streaming chat completion. Calls `on_event` for every reasoning/content piece.
    /// When `cancel` becomes true, stops reading, drops the connection (llama-server then
    /// aborts generation) and returns the partial result with `cancelled = true`.
    /// HTTP errors return `Err` with the server's error message.
    ///
    /// Always streams, whatever `params.stream` says. The cancel flag is honoured within about
    /// 10 ms, also while llama-server is still processing the prompt and sends nothing. A
    /// connection that ends before the answer is finished (server stopped) is an error.
    pub fn chat_stream(
        &self,
        messages: &[ChatMessage],
        params: &GenParams,
        cancel: &AtomicBool,
        on_event: &mut dyn FnMut(StreamEvent),
    ) -> anyhow::Result<ChatResult> {
        let started = Instant::now();
        let endpoint = Endpoint::parse(&self.base)?;
        let body = serde_json::to_vec(&build_chat_body(messages, params, true)).context("encoding the chat request")?;
        let request = endpoint.post_request("/v1/chat/completions", &body);
        drop(body);

        let mut result = if cancel.load(Ordering::Relaxed) {
            cancelled_result(ChatResult::default())
        } else {
            let mut stream = endpoint.connect()?;
            let outcome = stream_exchange(&mut stream, &request, cancel, on_event);
            // Close in every case; after a cancel this is what makes llama-server stop.
            let _ = stream.shutdown(Shutdown::Both);
            outcome?
        };
        result.elapsed_ms = elapsed_ms(started);
        Ok(result)
    }

    /// Full URL of an endpoint path (`/health`).
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// `GET path` and parse the JSON answer.
    fn get_json(&self, path: &str, timeout: Duration) -> anyhow::Result<Value> {
        let response = self.agent.get(&self.url(path)).timeout(timeout).call();
        self.read_json(path, response)
    }

    /// `POST path` with a JSON body and parse the JSON answer (`timeout: None` waits forever).
    fn post_json(&self, path: &str, body: &Value, timeout: Option<Duration>) -> anyhow::Result<Value> {
        let mut request = self.agent.post(&self.url(path));
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let response = request.send_json(body);
        self.read_json(path, response)
    }

    /// Turn a ureq outcome into JSON or a readable error.
    fn read_json(&self, path: &str, response: Result<ureq::Response, ureq::Error>) -> anyhow::Result<Value> {
        match response {
            Ok(response) => {
                let text = response.into_string().with_context(|| format!("reading Bonsai's answer to {path}"))?;
                serde_json::from_str(&text).with_context(|| format!("Bonsai's answer to {path} is not valid JSON"))
            }
            Err(ureq::Error::Status(code, response)) => Err(status_error_from_response(code, response)),
            Err(ureq::Error::Transport(transport)) => {
                Err(anyhow!("cannot reach Bonsai at {}: {}", self.base, transport_message(&transport)))
            }
        }
    }
}

/// Parse one SSE line (`data: {...}`, `data: [DONE]`, blank or comment lines -> `None`).
///
/// Also understood: `data:` without a space, a trailing `\r`, and `error: {...}` event lines.
/// Other fields (`event:`, `id:`, `retry:`) and `data:` values that are not JSON give `None`.
pub fn parse_sse_line(line: &str) -> Option<SseChunk> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() || line.starts_with(':') {
        return None;
    }
    let (field, value) = match line.split_once(':') {
        Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
        None => (line, ""),
    };
    let value = value.trim();
    match field {
        "data" => {
            if value == "[DONE]" {
                return Some(SseChunk { done: true, ..SseChunk::default() });
            }
            let json: Value = serde_json::from_str(value).ok()?;
            Some(chunk_from_json(&json))
        }
        "error" => {
            let message = match serde_json::from_str::<Value>(value) {
                Ok(json) => error_message(json.get("error").unwrap_or(&json)),
                Err(_) => value.to_string(),
            };
            Some(SseChunk { error: Some(non_empty_error(message)), ..SseChunk::default() })
        }
        _ => None,
    }
}

/// The parts of one parsed `data:` JSON object.
fn chunk_from_json(json: &Value) -> SseChunk {
    let mut chunk = SseChunk::default();
    if let Some(error) = json.get("error").filter(|e| !e.is_null()) {
        chunk.error = Some(non_empty_error(error_message(error)));
        return chunk;
    }
    if let Some(choices) = json.get("choices").and_then(Value::as_array) {
        let delta = choices.first().and_then(|choice| choice.get("delta").or_else(|| choice.get("message")));
        if let Some(delta) = delta {
            chunk.reasoning = delta.get("reasoning_content").and_then(Value::as_str).map(str::to_string);
            chunk.content = delta.get("content").and_then(Value::as_str).map(str::to_string);
        }
        chunk.finish_reason = choices
            .iter()
            .rev()
            .find_map(|choice| choice.get("finish_reason").and_then(Value::as_str))
            .map(str::to_string);
    }
    chunk.usage = json.get("usage").and_then(usage_from_json);
    chunk.timings = json.get("timings").and_then(timings_from_json);
    chunk
}

/// A complete (non-streamed) chat completion.
fn completion_from_json(json: &Value) -> anyhow::Result<ChatResult> {
    if let Some(error) = json.get("error").filter(|e| !e.is_null()) {
        bail!("Bonsai error: {}", non_empty_error(error_message(error)));
    }
    let choice = json
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .context("Bonsai's answer contains no choices")?;
    let message_text = |key: &str| {
        choice
            .get("message")
            .and_then(|message| message.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(ChatResult {
        content: message_text("content"),
        reasoning: message_text("reasoning_content"),
        finish_reason: string_field(choice, "finish_reason"),
        timings: json.get("timings").and_then(timings_from_json),
        usage: json.get("usage").and_then(usage_from_json),
        elapsed_ms: 0,
        cancelled: false,
    })
}

/// `usage` object -> [`Usage`] (missing numbers are 0).
fn usage_from_json(json: &Value) -> Option<Usage> {
    if !json.is_object() {
        return None;
    }
    let prompt_tokens = u32_field(json, "prompt_tokens");
    let completion_tokens = u32_field(json, "completion_tokens");
    let total_tokens =
        json.get("total_tokens").and_then(json_u32).unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    let cached_tokens = json.get("prompt_tokens_details").map_or(0, |details| u32_field(details, "cached_tokens"));
    Some(Usage { prompt_tokens, completion_tokens, total_tokens, cached_tokens })
}

/// `timings` object -> [`Timings`] (missing numbers are 0).
fn timings_from_json(json: &Value) -> Option<Timings> {
    if !json.is_object() {
        return None;
    }
    Some(Timings {
        prompt_n: u32_field(json, "prompt_n"),
        cache_n: u32_field(json, "cache_n"),
        prompt_ms: f64_field(json, "prompt_ms"),
        prompt_per_second: f64_field(json, "prompt_per_second"),
        predicted_n: u32_field(json, "predicted_n"),
        predicted_ms: f64_field(json, "predicted_ms"),
        predicted_per_second: f64_field(json, "predicted_per_second"),
    })
}

/// `/props` answer -> [`ServerProps`] (missing fields keep their defaults).
fn props_from_json(json: &Value) -> ServerProps {
    let model_path = json
        .get("model_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .or_else(|| json.get("model_alias").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let n_ctx = json
        .get("default_generation_settings")
        .and_then(|settings| settings.get("n_ctx"))
        .and_then(json_u32)
        .or_else(|| json.get("n_ctx").and_then(json_u32))
        .unwrap_or(0);
    let vision = json
        .get("modalities")
        .and_then(|modalities| modalities.get("vision"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ServerProps {
        model_name: file_name(&model_path),
        model_path,
        model_ftype: string_field(json, "model_ftype"),
        n_ctx,
        total_slots: u32_field(json, "total_slots"),
        vision,
        build_info: string_field(json, "build_info"),
    }
}

/// `/slots` answer -> [`SlotsActivity`]; `None` when it is not a list.
fn slots_activity_from_json(json: &Value) -> Option<SlotsActivity> {
    let slots = json.as_array()?;
    let tasks = slots
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let int = |value: Option<&Value>| value.and_then(Value::as_i64).unwrap_or(-1);
            let decoded =
                slot.get("next_token").and_then(|next| next.get("n_decoded")).or_else(|| slot.get("n_decoded"));
            (slot.get("id").and_then(Value::as_i64).unwrap_or(index as i64), int(slot.get("id_task")), int(decoded))
        })
        .collect();
    Some(SlotsActivity { processing: slots.iter().any(slot_is_processing), tasks })
}

/// One `/slots` entry: busy when `is_processing` (older builds: `state != 0`).
fn slot_is_processing(slot: &Value) -> bool {
    match slot.get("is_processing").and_then(Value::as_bool) {
        Some(processing) => processing,
        None => slot.get("state").and_then(Value::as_u64).is_some_and(|state| state != 0),
    }
}

/// Last component of a Windows or Unix path.
fn file_name(path: &str) -> String {
    path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
}

/// A non-negative JSON number as `u32` (floats are truncated, huge values saturate).
fn json_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64))
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn u32_field(json: &Value, key: &str) -> u32 {
    json.get(key).and_then(json_u32).unwrap_or(0)
}

fn f64_field(json: &Value, key: &str) -> f64 {
    json.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn string_field(json: &Value, key: &str) -> String {
    json.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Text of an `error` value: a string, or an object's `message`.
fn error_message(error: &Value) -> String {
    match error {
        Value::Null => String::new(),
        Value::String(text) => text.trim().to_string(),
        Value::Object(map) => match map.get("message").and_then(Value::as_str) {
            Some(message) => message.trim().to_string(),
            None => error.to_string(),
        },
        other => other.to_string(),
    }
}

fn non_empty_error(message: String) -> String {
    if message.is_empty() {
        "unknown error".to_string()
    } else {
        message
    }
}

/// `Bonsai error <code>: <message>` from a ureq error response.
fn status_error_from_response(code: u16, response: ureq::Response) -> anyhow::Error {
    let reason = response.status_text().to_string();
    let body = response.into_string().unwrap_or_default();
    status_error(code, &reason, &body)
}

/// `Bonsai error <code>: <message>`: the message comes from llama-server's
/// `{"error":{"message":..}}` body, else the body text, else the HTTP reason phrase.
fn status_error(code: u16, reason: &str, body: &str) -> anyhow::Error {
    anyhow!("Bonsai error {code}: {}", status_message(reason, body))
}

fn status_message(reason: &str, body: &str) -> String {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        if let Some(error) = json.get("error").filter(|e| !e.is_null()) {
            let message = error_message(error);
            if !message.is_empty() {
                return message;
            }
        }
        if let Some(message) = json.get("message").and_then(Value::as_str).filter(|m| !m.trim().is_empty()) {
            return message.trim().to_string();
        }
    }
    let text = body.trim();
    if !text.is_empty() {
        return shorten(text, MAX_ERROR_CHARS);
    }
    let reason = reason.trim();
    if reason.is_empty() {
        "no details".to_string()
    } else {
        reason.to_string()
    }
}

/// ureq transport error without the URL, e.g. `Connection Failed: Connect error: ...`.
fn transport_message(transport: &ureq::Transport) -> String {
    let mut message = transport.kind().to_string();
    if let Some(detail) = transport.message() {
        message.push_str(": ");
        message.push_str(detail);
    }
    if let Some(source) = std::error::Error::source(transport) {
        message.push_str(": ");
        message.push_str(&source.to_string());
    }
    message
}

/// At most `max_chars` characters of `text`, with `...` when cut.
fn shorten(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().nth(max_chars).is_some() {
        out.push_str("...");
    }
    out
}

fn user_agent() -> String {
    format!("bonsai-h3-writer/{}", crate::VERSION)
}

/// Trimmed URL without trailing slashes; empty -> the default, no scheme -> `http://`.
fn normalize_base_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        DEFAULT_BASE_URL.to_string()
    } else if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

/// ureq resolver: the system's addresses with IPv4 first (see the module docs).
fn resolve_ipv4_first(netloc: &str) -> io::Result<Vec<SocketAddr>> {
    let mut addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
    addrs.sort_by_key(|addr| !addr.is_ipv4());
    Ok(addrs)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn cancelled_result(mut partial: ChatResult) -> ChatResult {
    partial.cancelled = true;
    partial.finish_reason = "cancelled".to_string();
    partial
}

// ---------------------------------------------------------------------------------------------
// Raw HTTP/1.1 streaming
// ---------------------------------------------------------------------------------------------

/// Internal marker error: the caller's cancel flag was seen. Never returned to callers; it is
/// turned into a partial [`ChatResult`] with `cancelled = true`.
#[derive(Debug)]
struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Host, port and path prefix of the server, parsed from the base URL (plain `http` only).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    host: String,
    port: u16,
    /// Path of the base URL without trailing slash (`""` for `http://127.0.0.1:8080`).
    prefix: String,
}

impl Endpoint {
    fn parse(base: &str) -> anyhow::Result<Self> {
        let base = base.trim();
        let rest = match base.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
            Some((scheme, _)) if scheme.eq_ignore_ascii_case("https") => bail!(
                "https is not supported for the Bonsai server URL '{base}': llama-server listens on plain http (e.g. http://127.0.0.1:8080)"
            ),
            Some((scheme, _)) => {
                bail!("unsupported scheme '{scheme}' in the Bonsai server URL '{base}' (use http://host:port)")
            }
            None => base,
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        let authority = authority.split(['?', '#']).next().unwrap_or_default();
        let path = path.split(['?', '#']).next().unwrap_or_default();
        // Drop `user:password@`.
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = split_host_port(authority).with_context(|| format!("invalid Bonsai server URL '{base}'"))?;
        if host.is_empty() {
            bail!("no host in the Bonsai server URL '{base}'");
        }
        Ok(Self { host, port, prefix: path.trim_end_matches('/').to_string() })
    }

    /// `Host` header value (`127.0.0.1:8080`, `[::1]:8080`).
    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Connect with the usual timeout, IPv4 addresses first.
    fn connect(&self) -> anyhow::Result<TcpStream> {
        let mut addrs: Vec<SocketAddr> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .with_context(|| format!("cannot resolve the Bonsai host '{}'", self.host))?
            .collect();
        // llama-server listens on IPv4 by default; `localhost` may resolve to ::1 first, and a
        // refused connect costs about 2 s on Windows.
        addrs.sort_by_key(|addr| !addr.is_ipv4());
        let mut last_error = None;
        for addr in &addrs {
            match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
                Ok(stream) => return Ok(stream),
                Err(err) => last_error = Some(err),
            }
        }
        match last_error {
            Some(err) => Err(anyhow!("cannot reach Bonsai at http://{}: {err}", self.host_header())),
            None => bail!("the Bonsai host '{}' resolved to no address", self.host),
        }
    }

    /// Complete `POST` request (head + JSON body) asking for an event stream.
    fn post_request(&self, path: &str, body: &[u8]) -> Vec<u8> {
        let head = format!(
            "POST {prefix}{path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             User-Agent: {agent}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {length}\r\n\
             Accept: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n",
            prefix = self.prefix,
            host = self.host_header(),
            agent = user_agent(),
            length = body.len(),
        );
        let mut request = Vec::with_capacity(head.len() + body.len());
        request.extend_from_slice(head.as_bytes());
        request.extend_from_slice(body);
        request
    }
}

/// `host[:port]` or `[v6][:port]`; the port defaults to 80.
fn split_host_port(authority: &str) -> anyhow::Result<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').context("missing ']' after the IPv6 address")?;
        let port = match after.strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None if after.is_empty() => 80,
            None => bail!("unexpected '{after}' after the IPv6 address"),
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_string(), parse_port(port)?)),
        None => Ok((authority.to_string(), 80)),
    }
}

fn parse_port(text: &str) -> anyhow::Result<u16> {
    if text.is_empty() {
        return Ok(80);
    }
    text.parse::<u16>().with_context(|| format!("invalid port '{text}'"))
}

/// Send the request and read the streamed answer on a non-blocking socket.
fn stream_exchange(
    stream: &mut TcpStream,
    request: &[u8],
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<ChatResult> {
    stream.set_nonblocking(true).context("setting up the connection to Bonsai")?;
    let _ = stream.set_nodelay(true);
    match send_all(stream, request, cancel) {
        Ok(()) => {}
        Err(err) if err.is::<Cancelled>() => return Ok(cancelled_result(ChatResult::default())),
        Err(err) => return Err(err),
    }
    read_chat_response(stream, cancel, on_event)
}

/// Write all of `data` to a non-blocking writer, checking `cancel` between attempts.
fn send_all<W: Write>(writer: &mut W, data: &[u8], cancel: &AtomicBool) -> anyhow::Result<()> {
    let mut sent = 0;
    let mut last_progress = Instant::now();
    while sent < data.len() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        match writer.write(&data[sent..]) {
            Ok(0) => bail!("Bonsai closed the connection while the request was being sent"),
            Ok(n) => {
                sent += n;
                last_progress = Instant::now();
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if last_progress.elapsed() >= SEND_STALL_TIMEOUT {
                    bail!("Bonsai accepted no request data for {} s", SEND_STALL_TIMEOUT.as_secs());
                }
                std::thread::sleep(SEND_RETRY_PAUSE);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => bail!("sending the request to Bonsai failed: {err}"),
        }
    }
    let _ = writer.flush();
    Ok(())
}

/// What a read produced.
enum Step {
    Data(usize),
    Eof,
}

/// Read the next bytes. When no data is ready (`WouldBlock` on a non-blocking socket, or a
/// read timeout), pause briefly, check the cancel flag and try again.
fn read_step<R: Read>(reader: &mut R, buf: &mut [u8], cancel: &AtomicBool) -> anyhow::Result<Step> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        match reader.read(buf) {
            Ok(0) => return Ok(Step::Eof),
            Ok(n) => return Ok(Step::Data(n)),
            Err(err) => match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => std::thread::sleep(STREAM_IDLE_PAUSE),
                io::ErrorKind::Interrupted => {}
                _ => bail!("lost the connection to Bonsai: {err}"),
            },
        }
    }
}

/// Read one HTTP response carrying a chat answer (SSE stream, or plain JSON) and collect it.
/// A cancel gives `Ok` with the partial answer and `cancelled = true`.
fn read_chat_response<R: Read>(
    reader: &mut R,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<ChatResult> {
    let mut collector = SseCollector::default();
    match drive_response(reader, cancel, &mut collector, on_event) {
        Ok(()) => collector.finish(),
        Err(err) if err.is::<Cancelled>() => Ok(cancelled_result(collector.result)),
        Err(err) => Err(err),
    }
}

fn drive_response<R: Read>(
    reader: &mut R,
    cancel: &AtomicBool,
    collector: &mut SseCollector,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; READ_BUF_BYTES];
    let mut pending = Vec::new();
    let head = read_head(reader, &mut buf, &mut pending, cancel)?;
    let mut body = BodyDecoder::for_head(&head);

    if !(200..300).contains(&head.status) {
        let text = match read_body(reader, &mut buf, &pending, &mut body, cancel, MAX_ERROR_BODY) {
            Ok(mut bytes) => {
                bytes.truncate(MAX_ERROR_BODY);
                String::from_utf8_lossy(&bytes).into_owned()
            }
            Err(err) if err.is::<Cancelled>() => return Err(err),
            Err(_) => String::new(),
        };
        return Err(status_error(head.status, &head.reason, &text));
    }

    if head.is_json() {
        // Not streamed (e.g. a proxy or server that ignores `stream`): one complete answer.
        let bytes = read_body(reader, &mut buf, &pending, &mut body, cancel, MAX_BODY_BYTES)?;
        if bytes.len() >= MAX_BODY_BYTES && !body.is_complete() {
            bail!("Bonsai's answer is larger than {} MB", MAX_BODY_BYTES >> 20);
        }
        let json: Value = serde_json::from_slice(&bytes).context("Bonsai's answer is not valid JSON")?;
        collector.set_complete(completion_from_json(&json)?, on_event);
        return Ok(());
    }

    let mut decoded = Vec::new();
    body.feed(&pending, &mut decoded)?;
    collector.feed(&decoded, cancel, on_event)?;
    while !collector.done && !body.is_complete() {
        match read_step(reader, &mut buf, cancel)? {
            Step::Data(n) => {
                decoded.clear();
                body.feed(&buf[..n], &mut decoded)?;
                collector.feed(&decoded, cancel, on_event)?;
            }
            Step::Eof => break,
        }
    }
    collector.flush(on_event)?;
    if collector.done && !body.is_complete() {
        drain(reader, &mut buf, &mut body);
    }
    Ok(())
}

/// Read until the end of the response headers; `pending` keeps the bytes read past them.
/// Interim `1xx` responses are skipped.
fn read_head<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    pending: &mut Vec<u8>,
    cancel: &AtomicBool,
) -> anyhow::Result<ResponseHead> {
    loop {
        if let Some(end) = find_head_end(pending) {
            let head = parse_response_head(&pending[..end])?;
            pending.drain(..end);
            if (100..200).contains(&head.status) {
                continue;
            }
            return Ok(head);
        }
        if pending.len() > MAX_HEAD_BYTES {
            bail!("Bonsai sent response headers larger than {} KB", MAX_HEAD_BYTES >> 10);
        }
        match read_step(reader, buf, cancel)? {
            Step::Data(n) => pending.extend_from_slice(&buf[..n]),
            Step::Eof if pending.is_empty() => {
                bail!("Bonsai closed the connection without answering (was the server stopped?)")
            }
            Step::Eof => bail!("Bonsai closed the connection in the middle of the response headers"),
        }
    }
}

/// Collect a body (starting with `pending`) until it is complete, the connection closes, or at
/// least `limit` bytes are there.
fn read_body<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    pending: &[u8],
    body: &mut BodyDecoder,
    cancel: &AtomicBool,
    limit: usize,
) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    body.feed(pending, &mut out)?;
    while !body.is_complete() && out.len() < limit {
        match read_step(reader, buf, cancel)? {
            Step::Data(n) => body.feed(&buf[..n], &mut out)?,
            Step::Eof => break,
        }
    }
    Ok(out)
}

/// After `data: [DONE]`: read the end of the HTTP body (normally just the last empty chunk) for
/// a moment, so the connection is closed cleanly instead of with a reset.
fn drain<R: Read>(reader: &mut R, buf: &mut [u8], body: &mut BodyDecoder) {
    let deadline = Instant::now() + DONE_DRAIN_TIMEOUT;
    let mut sink = Vec::new();
    while !body.is_complete() && Instant::now() < deadline {
        match reader.read(buf) {
            Ok(0) => break,
            Ok(n) => {
                sink.clear();
                if body.feed(&buf[..n], &mut sink).is_err() {
                    break;
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(STREAM_IDLE_PAUSE)
            }
            Err(_) => break,
        }
    }
}

/// Status line and headers of an HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResponseHead {
    status: u16,
    reason: String,
    /// `(lower-case name, value)` pairs.
    headers: Vec<(String, String)>,
}

impl ResponseHead {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    fn is_json(&self) -> bool {
        self.header("content-type").is_some_and(|value| value.to_ascii_lowercase().contains("application/json"))
    }
}

/// Index just past the empty line that ends the headers (`\r\n\r\n` or `\n\n`).
fn find_head_end(data: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = data[from..].iter().position(|&b| b == b'\n') {
        let newline = from + offset;
        let rest = &data[newline + 1..];
        if rest.starts_with(b"\n") {
            return Some(newline + 2);
        }
        if rest.starts_with(b"\r\n") {
            return Some(newline + 3);
        }
        from = newline + 1;
    }
    None
}

fn parse_response_head(raw: &[u8]) -> anyhow::Result<ResponseHead> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.lines();
    let status_line = lines.next().unwrap_or_default().trim();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        bail!("Bonsai did not answer with HTTP (got {:?})", shorten(status_line, 80));
    }
    let status = parts
        .next()
        .unwrap_or_default()
        .parse::<u16>()
        .with_context(|| format!("Bonsai sent an invalid HTTP status line {:?}", shorten(status_line, 80)))?;
    let reason = parts.next().unwrap_or_default().trim().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Ok(ResponseHead { status, reason, headers })
}

/// How the response body is framed.
#[derive(Debug)]
enum BodyDecoder {
    Chunked(ChunkedDecoder),
    /// `Content-Length`: bytes still expected.
    Length(u64),
    /// Neither: the body ends when the connection closes.
    UntilEof,
}

impl BodyDecoder {
    fn for_head(head: &ResponseHead) -> Self {
        let chunked =
            head.header("transfer-encoding").is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
        if chunked {
            return BodyDecoder::Chunked(ChunkedDecoder::default());
        }
        if matches!(head.status, 204 | 304) {
            return BodyDecoder::Length(0);
        }
        match head.header("content-length").and_then(|value| value.trim().parse::<u64>().ok()) {
            Some(length) => BodyDecoder::Length(length),
            None => BodyDecoder::UntilEof,
        }
    }

    /// Append the body bytes contained in `input` to `out`.
    fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
        match self {
            BodyDecoder::Chunked(decoder) => decoder.feed(input, out),
            BodyDecoder::Length(left) => {
                let take = usize::try_from(*left).map_or(input.len(), |left| left.min(input.len()));
                out.extend_from_slice(&input[..take]);
                *left -= take as u64;
                Ok(())
            }
            BodyDecoder::UntilEof => {
                out.extend_from_slice(input);
                Ok(())
            }
        }
    }

    fn is_complete(&self) -> bool {
        match self {
            BodyDecoder::Chunked(decoder) => decoder.is_done(),
            BodyDecoder::Length(left) => *left == 0,
            BodyDecoder::UntilEof => false,
        }
    }
}

/// Incremental decoder for `Transfer-Encoding: chunked`; the input may be split anywhere.
#[derive(Debug, Default)]
struct ChunkedDecoder {
    state: ChunkState,
    /// Bytes of the current size / CRLF / trailer line.
    line: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ChunkState {
    /// Reading a `<hex size>[;extension]` line.
    #[default]
    Size,
    /// Copying chunk data; bytes still to come.
    Data(u64),
    /// Expecting the line break that ends a chunk's data.
    DataEnd,
    /// After the last (empty) chunk: trailer lines until an empty one.
    Trailer,
    /// Body complete; further input is ignored.
    Done,
}

impl ChunkedDecoder {
    /// Longest accepted size / trailer line.
    const MAX_LINE: usize = 4096;

    fn is_done(&self) -> bool {
        self.state == ChunkState::Done
    }

    /// Decode `input`, appending the chunk data to `out`.
    fn feed(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
        while !input.is_empty() {
            match self.state {
                ChunkState::Done => break,
                ChunkState::Data(left) => {
                    let take = usize::try_from(left).map_or(input.len(), |left| left.min(input.len()));
                    out.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    let left = left - take as u64;
                    self.state = if left == 0 { ChunkState::DataEnd } else { ChunkState::Data(left) };
                }
                ChunkState::Size | ChunkState::DataEnd | ChunkState::Trailer => {
                    let newline = input.iter().position(|&b| b == b'\n');
                    let part = newline.map_or(input, |at| &input[..at]);
                    if self.line.len() + part.len() > Self::MAX_LINE {
                        bail!("malformed chunked answer from Bonsai (control line too long)");
                    }
                    self.line.extend_from_slice(part);
                    match newline {
                        Some(at) => {
                            input = &input[at + 1..];
                            let line = std::mem::take(&mut self.line);
                            self.end_line(&line)?;
                        }
                        None => input = &[],
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle one complete control line (without its `\n`).
    fn end_line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        match self.state {
            ChunkState::Size => {
                let text = String::from_utf8_lossy(line);
                let size_text = text.split(';').next().unwrap_or_default().trim();
                let size = u64::from_str_radix(size_text, 16).map_err(|_| {
                    anyhow!("malformed chunked answer from Bonsai (bad chunk size {:?})", shorten(&text, 40))
                })?;
                self.state = if size == 0 { ChunkState::Trailer } else { ChunkState::Data(size) };
            }
            ChunkState::DataEnd => {
                if !line.is_empty() {
                    bail!("malformed chunked answer from Bonsai (chunk longer than announced)");
                }
                self.state = ChunkState::Size;
            }
            ChunkState::Trailer => {
                if line.is_empty() {
                    self.state = ChunkState::Done;
                }
            }
            ChunkState::Data(_) | ChunkState::Done => {}
        }
        Ok(())
    }
}

/// Collects the answer from the decoded SSE body, line by line.
#[derive(Debug, Default)]
struct SseCollector {
    /// Bytes of the current, unfinished line.
    line: Vec<u8>,
    result: ChatResult,
    /// `data: [DONE]` seen (or a complete JSON answer taken).
    done: bool,
}

impl SseCollector {
    /// Feed decoded body bytes; every complete line is parsed and applied.
    fn feed(&mut self, data: &[u8], cancel: &AtomicBool, on_event: &mut dyn FnMut(StreamEvent)) -> anyhow::Result<()> {
        let mut rest = data;
        while !self.done {
            let Some(newline) = rest.iter().position(|&b| b == b'\n') else {
                break;
            };
            self.line.extend_from_slice(&rest[..newline]);
            rest = &rest[newline + 1..];
            if cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            let line = std::mem::take(&mut self.line);
            self.apply(&line, on_event)?;
        }
        if !self.done {
            if self.line.len() + rest.len() > MAX_BODY_BYTES {
                bail!("Bonsai sent a stream line longer than {} MB", MAX_BODY_BYTES >> 20);
            }
            self.line.extend_from_slice(rest);
        }
        Ok(())
    }

    /// Apply a last line that had no line break before the connection closed.
    fn flush(&mut self, on_event: &mut dyn FnMut(StreamEvent)) -> anyhow::Result<()> {
        if !self.done && !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.apply(&line, on_event)?;
        }
        Ok(())
    }

    fn apply(&mut self, raw: &[u8], on_event: &mut dyn FnMut(StreamEvent)) -> anyhow::Result<()> {
        let text = String::from_utf8_lossy(raw);
        let Some(chunk) = parse_sse_line(&text) else {
            return Ok(());
        };
        if let Some(message) = chunk.error {
            bail!("Bonsai error: {message}");
        }
        if let Some(reasoning) = chunk.reasoning.filter(|piece| !piece.is_empty()) {
            self.result.reasoning.push_str(&reasoning);
            on_event(StreamEvent::Reasoning(reasoning));
        }
        if let Some(content) = chunk.content.filter(|piece| !piece.is_empty()) {
            self.result.content.push_str(&content);
            on_event(StreamEvent::Content(content));
        }
        if let Some(finish_reason) = chunk.finish_reason {
            self.result.finish_reason = finish_reason;
        }
        if chunk.usage.is_some() {
            self.result.usage = chunk.usage;
        }
        if chunk.timings.is_some() {
            self.result.timings = chunk.timings;
        }
        if chunk.done {
            self.done = true;
        }
        Ok(())
    }

    /// Take a complete, non-streamed answer.
    fn set_complete(&mut self, result: ChatResult, on_event: &mut dyn FnMut(StreamEvent)) {
        if !result.reasoning.is_empty() {
            on_event(StreamEvent::Reasoning(result.reasoning.clone()));
        }
        if !result.content.is_empty() {
            on_event(StreamEvent::Content(result.content.clone()));
        }
        self.result = result;
        self.done = true;
    }

    /// The answer once the response ended: an error when it stopped before the answer was
    /// finished (no `finish_reason`, no `[DONE]`).
    fn finish(self) -> anyhow::Result<ChatResult> {
        let finished = self.done || !self.result.finish_reason.is_empty();
        let result = self.result;
        if finished {
            return Ok(result);
        }
        let received = result.content.chars().count() + result.reasoning.chars().count();
        if received == 0 {
            bail!("Bonsai closed the connection without sending an answer (was the server stopped?)");
        }
        bail!("Bonsai stopped in the middle of the answer after {received} characters (was the server stopped?)")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::gen::ThinkingMode;

    // Real lines from the user's llama-server (b10787).
    const REASONING_LINE: &str = r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"reasoning_content":"We"}}],"object":"chat.completion.chunk"}"#;
    const CONTENT_LINE: &str = r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":" friend"}}],"object":"chat.completion.chunk"}"#;
    const FINISH_LINE: &str =
        r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"object":"chat.completion.chunk"}"#;
    const USAGE_LINE: &str = r#"data: {"choices":[],"usage":{"completion_tokens":4,"prompt_tokens":19,"total_tokens":23,"prompt_tokens_details":{"cached_tokens":0}},"timings":{"cache_n":0,"prompt_n":19,"prompt_ms":394.262,"prompt_per_token_ms":20.75,"prompt_per_second":48.19,"predicted_n":4,"predicted_ms":121.515,"predicted_per_token_ms":40.5,"predicted_per_second":24.69}}"#;
    const DONE_LINE: &str = "data: [DONE]";
    const ROLE_LINE: &str = r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"role":"assistant","content":null}}],"created":1793163401,"id":"chatcmpl-focasJAioYpV0awS11Ujk3IfmMjDw4oc","model":"bonsai","system_fingerprint":"b10787-73bfe6cc4","object":"chat.completion.chunk"}"#;

    const STREAM_HEAD: &str = "HTTP/1.1 200 OK\r\nServer: llama.cpp\r\nAccess-Control-Allow-Origin: \r\nX-Accel-Buffering: no\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

    // ----- parse_sse_line ---------------------------------------------------------------------

    #[test]
    fn sse_reasoning_piece() {
        assert_eq!(
            parse_sse_line(REASONING_LINE),
            Some(SseChunk { reasoning: Some("We".into()), ..SseChunk::default() })
        );
    }

    #[test]
    fn sse_content_piece() {
        assert_eq!(
            parse_sse_line(CONTENT_LINE),
            Some(SseChunk { content: Some(" friend".into()), ..SseChunk::default() })
        );
    }

    #[test]
    fn sse_finish_reason() {
        assert_eq!(
            parse_sse_line(FINISH_LINE),
            Some(SseChunk { finish_reason: Some("stop".into()), ..SseChunk::default() })
        );
    }

    #[test]
    fn sse_usage_and_timings() {
        let chunk = parse_sse_line(USAGE_LINE).expect("usage chunk");
        assert_eq!(
            chunk.usage,
            Some(Usage { prompt_tokens: 19, completion_tokens: 4, total_tokens: 23, cached_tokens: 0 })
        );
        let timings = chunk.timings.expect("timings");
        assert_eq!(timings.prompt_n, 19);
        assert_eq!(timings.cache_n, 0);
        assert_eq!(timings.predicted_n, 4);
        assert!((timings.prompt_ms - 394.262).abs() < 1e-9);
        assert!((timings.prompt_per_second - 48.19).abs() < 1e-9);
        assert!((timings.predicted_ms - 121.515).abs() < 1e-9);
        assert!((timings.predicted_per_second - 24.69).abs() < 1e-9);
        assert_eq!(chunk.content, None);
        assert_eq!(chunk.reasoning, None);
        assert_eq!(chunk.finish_reason, None);
        assert!(!chunk.done);
        assert_eq!(chunk.error, None);
    }

    #[test]
    fn sse_done_marker() {
        let done = Some(SseChunk { done: true, ..SseChunk::default() });
        assert_eq!(parse_sse_line(DONE_LINE), done);
        assert_eq!(parse_sse_line("data:[DONE]\r"), done);
        assert_eq!(parse_sse_line("data: [DONE]\r\n"), done);
    }

    #[test]
    fn sse_role_chunk_has_no_text() {
        assert_eq!(parse_sse_line(ROLE_LINE), Some(SseChunk::default()));
    }

    #[test]
    fn sse_without_space_and_with_crlf() {
        let line = format!("{}\r", CONTENT_LINE.replacen("data: ", "data:", 1));
        assert_eq!(parse_sse_line(&line).and_then(|c| c.content), Some(" friend".into()));
    }

    #[test]
    fn sse_ignores_blank_comment_and_other_lines() {
        for line in [
            "",
            "   ",
            "\r",
            "\n",
            ":",
            ": keep-alive",
            ":ping",
            "event: message",
            "id: 7",
            "retry: 1000",
            "data:",
            "data: ",
            "data: not json",
            "nonsense",
        ] {
            assert_eq!(parse_sse_line(line), None, "line {line:?}");
        }
    }

    #[test]
    fn sse_error_chunks() {
        let data_error =
            r#"data: {"error":{"code":500,"message":"Context size has been exceeded.","type":"server_error"}}"#;
        assert_eq!(
            parse_sse_line(data_error),
            Some(SseChunk { error: Some("Context size has been exceeded.".into()), ..SseChunk::default() })
        );
        let event_error = r#"error: {"code":500,"message":"boom","type":"server_error"}"#;
        assert_eq!(parse_sse_line(event_error).and_then(|c| c.error), Some("boom".into()));
        let wrapped_event = r#"error: {"error":{"message":"wrapped"}}"#;
        assert_eq!(parse_sse_line(wrapped_event).and_then(|c| c.error), Some("wrapped".into()));
        let plain = r#"data: {"error":"plain text"}"#;
        assert_eq!(parse_sse_line(plain).and_then(|c| c.error), Some("plain text".into()));
        assert_eq!(parse_sse_line("error: not json").and_then(|c| c.error), Some("not json".into()));
        // `"error": null` is not an error.
        assert_eq!(
            parse_sse_line(r#"data: {"error":null,"choices":[{"delta":{"content":"x"}}]}"#).and_then(|c| c.content),
            Some("x".into())
        );
    }

    // ----- chunked decoding -------------------------------------------------------------------

    /// Hand-made stream: chunk extension, a CRLF inside the data, trailers, junk after the end.
    const CHUNKED: &[u8] =
        b"4\r\nWiki\r\n5;name=value\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\nX-Trailer: yes\r\n\r\nJUNK";
    const UNCHUNKED: &[u8] = b"Wikipedia in\r\n\r\nchunks.";

    #[test]
    fn chunked_decoder_whole_input() {
        let mut decoder = ChunkedDecoder::default();
        let mut out = Vec::new();
        decoder.feed(CHUNKED, &mut out).expect("valid chunked data");
        assert!(decoder.is_done());
        assert_eq!(out, UNCHUNKED);
    }

    #[test]
    fn chunked_decoder_any_split_point() {
        for cut in 0..=CHUNKED.len() {
            let mut decoder = ChunkedDecoder::default();
            let mut out = Vec::new();
            decoder.feed(&CHUNKED[..cut], &mut out).expect("first part");
            decoder.feed(&CHUNKED[cut..], &mut out).expect("second part");
            assert!(decoder.is_done(), "cut at {cut}");
            assert_eq!(out, UNCHUNKED, "cut at {cut}");
        }
    }

    #[test]
    fn chunked_decoder_byte_by_byte_and_done_only_at_the_end() {
        let end = CHUNKED.len() - b"JUNK".len();
        let mut decoder = ChunkedDecoder::default();
        let mut out = Vec::new();
        for (index, byte) in CHUNKED.iter().enumerate() {
            assert_eq!(decoder.is_done(), index >= end, "byte {index}");
            decoder.feed(std::slice::from_ref(byte), &mut out).expect("byte");
        }
        assert!(decoder.is_done());
        assert_eq!(out, UNCHUNKED);
    }

    #[test]
    fn chunked_decoder_accepts_bare_lf() {
        let mut decoder = ChunkedDecoder::default();
        let mut out = Vec::new();
        decoder.feed(b"3\nabc\n0\n\n", &mut out).expect("LF-only framing");
        assert!(decoder.is_done());
        assert_eq!(out, b"abc");
    }

    #[test]
    fn chunked_decoder_rejects_bad_framing() {
        let bad_size = ChunkedDecoder::default().feed(b"zz\r\nabc\r\n", &mut Vec::new());
        assert!(bad_size.is_err());
        let empty_size = ChunkedDecoder::default().feed(b"\r\nabc\r\n", &mut Vec::new());
        assert!(empty_size.is_err());
        let too_long = ChunkedDecoder::default().feed(b"3\r\nabcd\r\n", &mut Vec::new());
        assert!(too_long.is_err());
        let endless_line = vec![b'1'; ChunkedDecoder::MAX_LINE + 1];
        assert!(ChunkedDecoder::default().feed(&endless_line, &mut Vec::new()).is_err());
    }

    // ----- response heads, endpoints, helpers -------------------------------------------------

    #[test]
    fn response_head_parsing() {
        let raw = format!("{STREAM_HEAD}data");
        let raw = raw.as_bytes();
        let end = find_head_end(raw).expect("head end");
        assert_eq!(&raw[end..], b"data");
        let head = parse_response_head(&raw[..end]).expect("head");
        assert_eq!(head.status, 200);
        assert_eq!(head.reason, "OK");
        assert_eq!(head.header("transfer-encoding"), Some("chunked"));
        assert_eq!(head.header("access-control-allow-origin"), Some(""));
        assert!(!head.is_json());
        assert!(matches!(BodyDecoder::for_head(&head), BodyDecoder::Chunked(_)));

        let lf_only = b"HTTP/1.0 404 Not Found\nContent-Type: application/json\nContent-Length: 5\n\nhello";
        let end = find_head_end(lf_only).expect("LF head end");
        let head = parse_response_head(&lf_only[..end]).expect("LF head");
        assert_eq!((head.status, head.reason.as_str()), (404, "Not Found"));
        assert!(head.is_json());
        assert!(matches!(BodyDecoder::for_head(&head), BodyDecoder::Length(5)));

        assert_eq!(find_head_end(b"HTTP/1.1 200 OK\r\nServer: x\r\n"), None);
        assert!(parse_response_head(b"SSH-2.0-OpenSSH\r\n\r\n").is_err());
        assert!(parse_response_head(b"HTTP/1.1 abc OK\r\n\r\n").is_err());
    }

    #[test]
    fn endpoint_parsing() {
        let endpoint = Endpoint::parse("http://127.0.0.1:8080").expect("plain");
        assert_eq!(endpoint, Endpoint { host: "127.0.0.1".into(), port: 8080, prefix: String::new() });
        assert_eq!(endpoint.host_header(), "127.0.0.1:8080");

        let endpoint = Endpoint::parse("http://localhost/").expect("default port");
        assert_eq!((endpoint.host.as_str(), endpoint.port, endpoint.prefix.as_str()), ("localhost", 80, ""));

        let endpoint = Endpoint::parse("HTTP://[::1]:9000/api/").expect("ipv6 + prefix");
        assert_eq!((endpoint.host.as_str(), endpoint.port, endpoint.prefix.as_str()), ("::1", 9000, "/api"));
        assert_eq!(endpoint.host_header(), "[::1]:9000");

        let endpoint = Endpoint::parse("127.0.0.1:8080").expect("no scheme");
        assert_eq!(endpoint.port, 8080);

        let https = Endpoint::parse("https://127.0.0.1:8080").expect_err("https");
        assert!(https.to_string().contains("https is not supported"), "{https}");
        assert!(Endpoint::parse("ftp://127.0.0.1").is_err());
        assert!(Endpoint::parse("http://127.0.0.1:notaport").is_err());
        assert!(Endpoint::parse("http://:8080").is_err());
    }

    #[test]
    fn request_head_is_well_formed() {
        let endpoint = Endpoint::parse("http://127.0.0.1:8080").expect("endpoint");
        let request = endpoint.post_request("/v1/chat/completions", b"{\"a\":1}");
        let text = String::from_utf8(request).expect("utf8");
        assert!(text.starts_with("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n"), "{text}");
        assert!(text.contains("\r\nContent-Type: application/json\r\n"));
        assert!(text.contains("\r\nContent-Length: 7\r\n"));
        assert!(text.contains("\r\nAccept: text/event-stream\r\n"));
        assert!(text.contains("\r\nConnection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n{\"a\":1}"));
    }

    #[test]
    fn base_url_normalization() {
        assert_eq!(BonsaiClient::new("http://127.0.0.1:8080/").base_url(), "http://127.0.0.1:8080");
        assert_eq!(BonsaiClient::new("  127.0.0.1:8080 ").base_url(), "http://127.0.0.1:8080");
        assert_eq!(BonsaiClient::new("").base_url(), DEFAULT_BASE_URL);
        assert_eq!(BonsaiClient::new("http://host:1/api//").base_url(), "http://host:1/api");
    }

    #[test]
    fn props_parsing() {
        let json = json!({
            "default_generation_settings": { "n_ctx": 16384 },
            "total_slots": 4,
            "model_path": "D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B\\Ternary-Bonsai-2-27B-PTQ1_0.gguf",
            "model_ftype": "PTQ1_0 - 1.75 bpw ternary (group 128)",
            "modalities": { "vision": true, "video": true, "audio": false },
            "build_info": "b10787-73bfe6cc4"
        });
        let props = props_from_json(&json);
        assert_eq!(props.model_name, "Ternary-Bonsai-2-27B-PTQ1_0.gguf");
        assert!(props.model_path.starts_with("D:\\Bonsai-demo\\"));
        assert_eq!(props.model_ftype, "PTQ1_0 - 1.75 bpw ternary (group 128)");
        assert_eq!(props.n_ctx, 16384);
        assert_eq!(props.total_slots, 4);
        assert!(props.vision);
        assert_eq!(props.build_info, "b10787-73bfe6cc4");

        assert_eq!(props_from_json(&json!({})), ServerProps::default());
        let unix = props_from_json(&json!({ "model_path": "/models/a/b.gguf", "n_ctx": 4096.0 }));
        assert_eq!((unix.model_name.as_str(), unix.n_ctx), ("b.gguf", 4096));
    }

    #[test]
    fn slot_state_detection() {
        assert!(slot_is_processing(&json!({ "id": 0, "is_processing": true })));
        assert!(!slot_is_processing(&json!({ "id": 0, "is_processing": false })));
        assert!(slot_is_processing(&json!({ "id": 0, "state": 1 })));
        assert!(!slot_is_processing(&json!({ "id": 0, "state": 0 })));
        assert!(!slot_is_processing(&json!({ "id": 0 })));
    }

    #[test]
    fn slots_activity_parsing() {
        // The shape of build b10787 (idle), trimmed.
        let idle = json!([
            { "id": 0, "id_task": 28018, "is_processing": false, "next_token": { "n_decoded": 0 } },
            { "id": 1, "id_task": 26727, "is_processing": false, "next_token": { "n_decoded": 0 } }
        ]);
        let before = slots_activity_from_json(&idle).expect("list");
        assert_eq!(before, SlotsActivity { processing: false, tasks: vec![(0, 28018, 0), (1, 26727, 0)] });
        assert!(!before.active_since(None) && !before.active_since(Some(&before)));
        // Another client's request ran on slot 1 in between: a new task id.
        let mut after = before.clone();
        after.tasks[1].1 = 28100;
        assert!(after.active_since(Some(&before)) && !after.active_since(None));
        let busy = slots_activity_from_json(&json!([{ "id": 0, "state": 1, "n_decoded": 7 }])).expect("list");
        assert_eq!(busy.tasks, vec![(0, -1, 7)]);
        assert!(busy.active_since(None), "older builds: state != 0");
        assert_eq!(slots_activity_from_json(&json!({ "error": "x" })), None);
    }

    #[test]
    fn resolver_puts_ipv4_first() {
        let addrs = resolve_ipv4_first("localhost:8080").expect("localhost resolves");
        assert!(!addrs.is_empty());
        if addrs.iter().any(SocketAddr::is_ipv4) {
            assert!(addrs[0].is_ipv4(), "{addrs:?}");
        }
        assert!(addrs.windows(2).all(|w| w[0].is_ipv4() >= w[1].is_ipv4()), "{addrs:?}");
    }

    #[test]
    fn status_error_messages() {
        let loading = r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#;
        assert_eq!(status_error(503, "Service Unavailable", loading).to_string(), "Bonsai error 503: Loading model");
        assert_eq!(
            status_error(500, "Internal Server Error", "").to_string(),
            "Bonsai error 500: Internal Server Error"
        );
        assert_eq!(
            status_error(502, "Bad Gateway", " <html>oops</html> ").to_string(),
            "Bonsai error 502: <html>oops</html>"
        );
        assert_eq!(status_error(500, "", r#"{"message":"top level"}"#).to_string(), "Bonsai error 500: top level");
        assert_eq!(status_error(500, "", "").to_string(), "Bonsai error 500: no details");
        let long = "x".repeat(MAX_ERROR_CHARS + 50);
        assert!(status_error(500, "", &long).to_string().ends_with("x..."));
    }

    #[test]
    fn completion_parsing() {
        let json = json!({
            "choices": [{ "finish_reason": "length", "index": 0,
                "message": { "role": "assistant", "content": "Hi there", "reasoning_content": "think" } }],
            "usage": { "completion_tokens": 2, "prompt_tokens": 10, "total_tokens": 12,
                "prompt_tokens_details": { "cached_tokens": 8 } },
            "timings": { "cache_n": 8, "prompt_n": 2, "prompt_ms": 40.0, "prompt_per_second": 50.0,
                "predicted_n": 2, "predicted_ms": 80.0, "predicted_per_second": 25.0 }
        });
        let result = completion_from_json(&json).expect("completion");
        assert_eq!(result.content, "Hi there");
        assert_eq!(result.reasoning, "think");
        assert_eq!(result.finish_reason, "length");
        assert_eq!(result.usage.as_ref().map(|u| u.cached_tokens), Some(8));
        assert_eq!(result.timings.as_ref().map(|t| t.cache_n), Some(8));
        assert!(!result.cancelled);

        let error = completion_from_json(&json!({ "error": { "message": "nope" } })).expect_err("error");
        assert_eq!(error.to_string(), "Bonsai error: nope");
        assert!(completion_from_json(&json!({ "choices": [] })).is_err());
        let sparse = completion_from_json(&json!({ "choices": [{ "message": { "content": null } }] })).expect("sparse");
        assert_eq!(sparse, ChatResult::default());
    }

    // ----- stream reading with a scripted reader ------------------------------------------------

    enum Piece {
        Bytes(Vec<u8>),
        /// No data yet (`WouldBlock`).
        Idle,
        Fail(io::ErrorKind),
    }

    /// A reader that plays back a script of byte pieces, idle moments and failures.
    #[derive(Default)]
    struct Script {
        pieces: VecDeque<Piece>,
        /// Answer `WouldBlock` forever once the script is played (instead of EOF).
        idle_when_empty: bool,
    }

    impl Script {
        fn bytes(mut self, data: &[u8]) -> Self {
            self.pieces.push_back(Piece::Bytes(data.to_vec()));
            self
        }
        fn idle(mut self) -> Self {
            self.pieces.push_back(Piece::Idle);
            self
        }
        fn fail(mut self, kind: io::ErrorKind) -> Self {
            self.pieces.push_back(Piece::Fail(kind));
            self
        }
        fn idle_forever(mut self) -> Self {
            self.idle_when_empty = true;
            self
        }
        /// `data` in pieces of `step` bytes, with a few idle moments in between.
        fn split(data: &[u8], step: usize) -> Self {
            let count = data.len().div_ceil(step);
            let mut script = Script::default();
            for (index, piece) in data.chunks(step).enumerate() {
                if index == 1 || index == count / 2 || index + 1 == count {
                    script = script.idle();
                }
                script = script.bytes(piece);
            }
            script
        }
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.pieces.pop_front() {
                Some(Piece::Bytes(mut data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    if n < data.len() {
                        self.pieces.push_front(Piece::Bytes(data.split_off(n)));
                    }
                    Ok(n)
                }
                Some(Piece::Idle) => Err(io::ErrorKind::WouldBlock.into()),
                Some(Piece::Fail(kind)) => Err(kind.into()),
                None if self.idle_when_empty => Err(io::ErrorKind::WouldBlock.into()),
                None => Ok(0),
            }
        }
    }

    fn delta_line(delta: Value) -> String {
        format!(
            "data: {}",
            json!({ "choices": [{ "finish_reason": null, "index": 0, "delta": delta }], "object": "chat.completion.chunk" })
        )
    }

    fn event(line: &str) -> String {
        format!("{line}\n\n")
    }

    /// Encode `pieces` as HTTP chunks, then the terminating empty chunk.
    fn chunked_body(pieces: &[String]) -> Vec<u8> {
        let mut out = Vec::new();
        for piece in pieces {
            out.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
            out.extend_from_slice(piece.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    /// The SSE events of a real answer ("Hi there friend"), grouped into chunks like llama-server.
    fn answer_events() -> Vec<String> {
        vec![
            event(ROLE_LINE) + &event(&delta_line(json!({ "content": "Hi" }))),
            event(&delta_line(json!({ "content": " there" }))),
            event(&delta_line(json!({ "content": " friend" }))),
            event(FINISH_LINE) + &event(USAGE_LINE),
            event(DONE_LINE),
        ]
    }

    /// A complete chunked streaming response as the server sends it.
    fn real_stream() -> Vec<u8> {
        let mut out = STREAM_HEAD.as_bytes().to_vec();
        out.extend(chunked_body(&answer_events()));
        out
    }

    fn content_events(texts: &[&str]) -> Vec<StreamEvent> {
        texts.iter().map(|t| StreamEvent::Content((*t).to_string())).collect()
    }

    fn run(reader: &mut impl Read) -> (anyhow::Result<ChatResult>, Vec<StreamEvent>) {
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let result = read_chat_response(reader, &cancel, &mut |event| events.push(event));
        (result, events)
    }

    #[test]
    fn stream_collects_answer_across_odd_reads() {
        let bytes = real_stream();
        for step in [1, 2, 7, 64, 333, bytes.len()] {
            let (result, events) = run(&mut Script::split(&bytes, step));
            let result = result.unwrap_or_else(|e| panic!("step {step}: {e:#}"));
            assert_eq!(result.content, "Hi there friend", "step {step}");
            assert_eq!(events, content_events(&["Hi", " there", " friend"]), "step {step}");
            assert_eq!(result.reasoning, "");
            assert_eq!(result.finish_reason, "stop");
            assert_eq!(result.usage.as_ref().map(|u| u.prompt_tokens), Some(19));
            assert_eq!(result.timings.as_ref().map(|t| t.predicted_n), Some(4));
            assert!(!result.cancelled);
        }
    }

    #[test]
    fn stream_reasoning_then_content() {
        let pieces = vec![
            event(REASONING_LINE),
            event(&delta_line(json!({ "reasoning_content": " think" }))),
            event(CONTENT_LINE),
            event(FINISH_LINE),
            event(DONE_LINE),
        ];
        let mut bytes = STREAM_HEAD.as_bytes().to_vec();
        bytes.extend(chunked_body(&pieces));
        let (result, events) = run(&mut Script::split(&bytes, 50));
        let result = result.expect("answer");
        assert_eq!(result.reasoning, "We think");
        assert_eq!(result.content, " friend");
        assert_eq!(
            events,
            vec![
                StreamEvent::Reasoning("We".into()),
                StreamEvent::Reasoning(" think".into()),
                StreamEvent::Content(" friend".into()),
            ]
        );
    }

    #[test]
    fn stream_without_framing_reads_until_eof() {
        let mut bytes = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".to_vec();
        bytes.extend(answer_events().concat().into_bytes());
        let (result, _) = run(&mut Script::split(&bytes, 100));
        assert_eq!(result.expect("answer").content, "Hi there friend");
    }

    #[test]
    fn stream_with_content_length() {
        let body = answer_events().concat();
        let head =
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n", body.len());
        // More bytes after the body must be ignored.
        let script = Script::default().bytes(head.as_bytes()).bytes(body.as_bytes()).bytes(b"junk");
        let (result, _) = run(&mut { script });
        assert_eq!(result.expect("answer").finish_reason, "stop");
    }

    #[test]
    fn stream_last_line_without_newline_is_used() {
        let body = event(&delta_line(json!({ "content": "Hi" }))) + FINISH_LINE;
        let mut bytes = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".to_vec();
        bytes.extend(body.into_bytes());
        let (result, _) = run(&mut Script::default().bytes(&bytes));
        let result = result.expect("answer");
        assert_eq!((result.content.as_str(), result.finish_reason.as_str()), ("Hi", "stop"));
    }

    #[test]
    fn stream_skips_interim_response() {
        let mut bytes = b"HTTP/1.1 100 Continue\r\n\r\n".to_vec();
        bytes.extend(real_stream());
        let (result, _) = run(&mut Script::default().bytes(&bytes));
        assert_eq!(result.expect("answer").content, "Hi there friend");
    }

    #[test]
    fn stream_http_error_status() {
        let body =
            r#"{"error":{"code":400,"message":"Expected 'messages' to be an array","type":"invalid_request_error"}}"#;
        let raw = format!(
            "HTTP/1.1 400 Bad Request\r\nServer: llama.cpp\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (result, events) = run(&mut Script::split(raw.as_bytes(), 40));
        let error = result.expect_err("400");
        assert_eq!(error.to_string(), "Bonsai error 400: Expected 'messages' to be an array");
        assert!(events.is_empty());
    }

    #[test]
    fn stream_error_event_mid_stream() {
        let pieces = vec![
            event(&delta_line(json!({ "content": "Hi" }))),
            event(r#"data: {"error":{"code":500,"message":"Context size has been exceeded.","type":"server_error"}}"#),
            event(DONE_LINE),
        ];
        let mut bytes = STREAM_HEAD.as_bytes().to_vec();
        bytes.extend(chunked_body(&pieces));
        let (result, events) = run(&mut Script::default().bytes(&bytes));
        let error = result.expect_err("stream error");
        assert_eq!(error.to_string(), "Bonsai error: Context size has been exceeded.");
        assert_eq!(events, content_events(&["Hi"]));
    }

    #[test]
    fn stream_cut_off_is_an_error() {
        let bytes = real_stream();
        let cut = bytes.windows(6).position(|w| w == b"there\"").expect("marker");
        let (result, _) = run(&mut Script::default().bytes(&bytes[..cut + 40]));
        let error = result.expect_err("cut stream").to_string();
        assert!(error.contains("middle of the answer"), "{error}");

        let (result, _) = run(&mut Script::default());
        let error = result.expect_err("no answer").to_string();
        assert!(error.contains("without answering"), "{error}");

        let (result, _) = run(&mut Script::default().bytes(STREAM_HEAD.as_bytes()));
        let error = result.expect_err("headers only").to_string();
        assert!(error.contains("without sending an answer"), "{error}");
    }

    #[test]
    fn stream_connection_reset_is_an_error() {
        let bytes = real_stream();
        let script = Script::default().bytes(&bytes[..STREAM_HEAD.len() + 20]).fail(io::ErrorKind::ConnectionReset);
        let (result, _) = run(&mut { script });
        let error = result.expect_err("reset").to_string();
        assert!(error.starts_with("lost the connection to Bonsai"), "{error}");
    }

    #[test]
    fn stream_malformed_chunks_are_an_error() {
        let mut bytes = STREAM_HEAD.as_bytes().to_vec();
        bytes.extend_from_slice(b"zz\r\ndata: x\r\n");
        let (result, _) = run(&mut Script::default().bytes(&bytes));
        assert!(result.expect_err("bad chunk").to_string().contains("malformed chunked"));
    }

    #[test]
    fn stream_plain_json_answer() {
        let body = r#"{"choices":[{"finish_reason":"stop","index":0,"message":{"role":"assistant","content":"Hello","reasoning_content":"hm"}}],"usage":{"completion_tokens":1,"prompt_tokens":5,"total_tokens":6}}"#;
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (result, events) = run(&mut Script::split(raw.as_bytes(), 30));
        let result = result.expect("json answer");
        assert_eq!((result.content.as_str(), result.reasoning.as_str()), ("Hello", "hm"));
        assert_eq!(result.usage.map(|u| u.total_tokens), Some(6));
        assert_eq!(events, vec![StreamEvent::Reasoning("hm".into()), StreamEvent::Content("Hello".into())]);
    }

    #[test]
    fn stream_cancel_after_first_piece_keeps_partial_answer() {
        // Everything arrives in one read; the cancel set in the callback stops the rest.
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let result = read_chat_response(&mut Script::default().bytes(&real_stream()), &cancel, &mut |event| {
            events.push(event);
            cancel.store(true, Ordering::Relaxed);
        })
        .expect("cancelled result");
        assert!(result.cancelled);
        assert_eq!(result.finish_reason, "cancelled");
        assert_eq!(result.content, "Hi");
        assert_eq!(events, content_events(&["Hi"]));
    }

    #[test]
    fn stream_cancel_while_waiting_for_headers() {
        // Like llama-server during prompt processing: no bytes at all.
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
            });
            read_chat_response(&mut Script::default().idle_forever(), &cancel, &mut |_| {})
        })
        .expect("cancelled result");
        assert!(result.cancelled);
        assert_eq!(result.finish_reason, "cancelled");
        assert!(result.content.is_empty());
        assert!(started.elapsed() < Duration::from_millis(600), "{:?}", started.elapsed());
    }

    #[test]
    fn send_all_handles_partial_writes_and_cancel() {
        // Accepts at most 3 bytes per call and says `WouldBlock` every other call.
        struct Slow {
            written: Vec<u8>,
            blocked: bool,
        }
        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.blocked = !self.blocked;
                if !self.blocked {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = buf.len().min(3);
                self.written.extend_from_slice(&buf[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let cancel = AtomicBool::new(false);
        let mut slow = Slow { written: Vec::new(), blocked: false };
        send_all(&mut slow, b"hello world", &cancel).expect("sent");
        assert_eq!(slow.written, b"hello world");

        cancel.store(true, Ordering::Relaxed);
        let error = send_all(&mut slow, b"more", &cancel).expect_err("cancelled");
        assert!(error.is::<Cancelled>());
    }

    // ----- against a local mock server (real sockets) ------------------------------------------

    /// Read one HTTP request (head + `Content-Length` body).
    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(end) = find_head_end(&data) {
                let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().expect("length"));
                if data.len() >= end + length {
                    return data;
                }
            }
            let n = stream.read(&mut buf).expect("read request");
            if n == 0 {
                return data;
            }
            data.extend_from_slice(&buf[..n]);
        }
    }

    /// Body of a recorded request as JSON.
    fn request_json(request: &[u8]) -> Value {
        let end = find_head_end(request).expect("request head");
        serde_json::from_slice(&request[end..]).expect("request body")
    }

    /// Serve one connection on a fresh localhost port: read the request, then run `handler`.
    fn serve_once<T, F>(handler: F) -> (String, thread::JoinHandle<T>)
    where
        T: Send + 'static,
        F: FnOnce(TcpStream, Vec<u8>) -> T + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_request(&mut stream);
            handler(stream, request)
        });
        (url, handle)
    }

    /// Answer one connection per canned response, in order; returns the requests.
    fn serve_all(responses: Vec<String>) -> (String, thread::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                requests.push(read_request(&mut stream));
                stream.write_all(response.as_bytes()).expect("write");
            }
            requests
        });
        (url, handle)
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nServer: llama.cpp\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn tiny_params() -> GenParams {
        GenParams { thinking: ThinkingMode::Off, temperature: 0.4, max_tokens: 20, ..GenParams::prompt_writer() }
    }

    #[test]
    fn mock_health_states() {
        let (url, server) = serve_all(vec![
            json_response("200 OK", r#"{"status":"ok"}"#),
            json_response(
                "503 Service Unavailable",
                r#"{"error":{"code":503,"message":"Loading model","type":"unavailable_error"}}"#,
            ),
            json_response(
                "500 Internal Server Error",
                r#"{"error":{"code":500,"message":"boom","type":"server_error"}}"#,
            ),
        ]);
        let client = BonsaiClient::new(&url);
        assert_eq!(client.health(), Health::Ready);
        assert_eq!(client.health(), Health::Loading);
        assert_eq!(client.health(), Health::Down("Bonsai error 500: boom".into()));
        let requests = server.join().expect("server");
        assert!(String::from_utf8_lossy(&requests[0]).starts_with("GET /health HTTP/1.1\r\n"));
    }

    #[test]
    fn mock_props_slots_tokenize_and_errors() {
        let props = r#"{"default_generation_settings":{"n_ctx":16384},"total_slots":4,"model_path":"D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B\\Ternary-Bonsai-2-27B-PTQ1_0.gguf","model_ftype":"PTQ1_0 - 1.75 bpw ternary (group 128)","modalities":{"vision":true,"video":true,"audio":false},"build_info":"b10787-73bfe6cc4"}"#;
        let slots = r#"[{"id":0,"n_ctx":16384,"is_processing":false},{"id":1,"n_ctx":16384,"is_processing":true}]"#;
        let tokens = r#"{"tokens":[9419,1814,11,821,4099,13]}"#;
        let not_found = r#"{"error":{"message":"File Not Found","type":"not_found_error","code":404}}"#;
        let (url, server) = serve_all(vec![
            json_response("200 OK", props),
            json_response("200 OK", slots),
            json_response("200 OK", tokens),
            json_response("404 Not Found", not_found),
            json_response("200 OK", "not json"),
        ]);
        let client = BonsaiClient::new(&format!("{url}/"));
        let props = client.props().expect("props");
        assert_eq!(props.model_name, "Ternary-Bonsai-2-27B-PTQ1_0.gguf");
        assert_eq!((props.n_ctx, props.total_slots, props.vision), (16384, 4, true));
        assert!(client.slots_busy().expect("slots"));
        assert_eq!(client.tokenize_count("Hello world, my friend.").expect("tokens"), 6);
        let error = client.props().expect_err("404");
        assert_eq!(error.to_string(), "Bonsai error 404: File Not Found");
        let error = client.props().expect_err("bad json");
        assert!(error.to_string().contains("not valid JSON"), "{error}");

        let requests = server.join().expect("server");
        assert!(String::from_utf8_lossy(&requests[0]).starts_with("GET /props HTTP/1.1\r\n"));
        assert!(String::from_utf8_lossy(&requests[1]).starts_with("GET /slots HTTP/1.1\r\n"));
        assert!(String::from_utf8_lossy(&requests[2]).starts_with("POST /tokenize HTTP/1.1\r\n"));
        assert_eq!(request_json(&requests[2]), json!({ "content": "Hello world, my friend." }));
    }

    #[test]
    fn mock_slots_activity_states() {
        let slots = r#"[{"id":0,"n_ctx":16384,"is_processing":true,"id_task":5,"next_token":{"n_decoded":12}}]"#;
        let no_slots = r#"{"error":{"code":501,"message":"This server does not support slots endpoint.","type":"not_supported_error"}}"#;
        let (url, server) = serve_all(vec![
            json_response("200 OK", slots),
            json_response("501 Not Implemented", no_slots),
            json_response("200 OK", "not json"),
        ]);
        // `localhost`: answered at once although it may resolve to ::1 first.
        let client = BonsaiClient::new(&url.replace("127.0.0.1", "localhost"));
        let started = Instant::now();
        let busy = client.slots_activity(Duration::from_secs(3)).expect("answer").expect("slots");
        assert!(started.elapsed() < Duration::from_millis(1000), "took {:?}", started.elapsed());
        assert_eq!(busy, SlotsActivity { processing: true, tasks: vec![(0, 5, 12)] });
        assert_eq!(client.slots_activity(Duration::from_secs(3)).expect("answer"), None, "no slots endpoint");
        assert!(client.slots_activity(Duration::from_secs(3)).is_err(), "garbage");
        let requests = server.join().expect("server");
        assert!(String::from_utf8_lossy(&requests[0]).starts_with("GET /slots HTTP/1.1\r\n"));
        // Nothing listening: an error (the idle stop then waits).
        let port = TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port();
        assert!(BonsaiClient::new(&format!("http://127.0.0.1:{port}")).slots_activity(Duration::from_secs(3)).is_err());
    }

    #[test]
    fn mock_non_streaming_chat() {
        let completion = r#"{"choices":[{"finish_reason":"stop","index":0,"message":{"role":"assistant","content":"Hi there"}}],"usage":{"completion_tokens":2,"prompt_tokens":10,"total_tokens":12,"prompt_tokens_details":{"cached_tokens":8}},"timings":{"cache_n":8,"prompt_n":2,"prompt_ms":40.0,"prompt_per_second":50.0,"predicted_n":2,"predicted_ms":80.0,"predicted_per_second":25.0}}"#;
        let bad =
            r#"{"error":{"code":400,"message":"Expected 'messages' to be an array","type":"invalid_request_error"}}"#;
        let (url, server) = serve_all(vec![json_response("200 OK", completion), json_response("400 Bad Request", bad)]);
        let client = BonsaiClient::new(&url);
        let result = client.chat(&[ChatMessage::user("hi")], &tiny_params()).expect("chat");
        assert_eq!(result.content, "Hi there");
        assert_eq!(result.finish_reason, "stop");
        assert_eq!(result.usage.map(|u| u.cached_tokens), Some(8));
        assert_eq!(result.timings.map(|t| t.prompt_n), Some(2));
        let error = client.chat(&[ChatMessage::user("hi")], &tiny_params()).expect_err("400");
        assert_eq!(error.to_string(), "Bonsai error 400: Expected 'messages' to be an array");

        let requests = server.join().expect("server");
        let body = request_json(&requests[0]);
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 20);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    }

    #[test]
    fn mock_chat_stream_end_to_end() {
        let response = real_stream();
        let (url, server) = serve_once(move |mut stream, request| {
            // Small pieces with pauses, so reads see partial chunks and empty polls.
            for piece in response.chunks(150) {
                stream.write_all(piece).expect("write");
                thread::sleep(Duration::from_millis(3));
            }
            request
        });
        let client = BonsaiClient::new(&url);
        let cancel = AtomicBool::new(false);
        let mut streamed = String::new();
        let result = client
            .chat_stream(&[ChatMessage::user("hi")], &tiny_params(), &cancel, &mut |event| {
                if let StreamEvent::Content(text) = event {
                    streamed.push_str(&text);
                }
            })
            .expect("stream");
        assert_eq!(result.content, "Hi there friend");
        assert_eq!(streamed, result.content);
        assert_eq!(result.finish_reason, "stop");
        assert!(result.usage.is_some() && result.timings.is_some());
        assert!(!result.cancelled);

        let request = server.join().expect("server");
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"), "{text}");
        assert!(text.contains(&format!("\r\nHost: {}\r\n", url.trim_start_matches("http://"))));
        assert!(text.contains("\r\nAccept: text/event-stream\r\n"));
        let body = request_json(&request);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[test]
    fn mock_chat_stream_http_error() {
        let bad =
            r#"{"error":{"code":400,"message":"Failed to load image or audio file","type":"invalid_request_error"}}"#;
        let (url, server) = serve_all(vec![json_response("400 Bad Request", bad)]);
        let cancel = AtomicBool::new(false);
        let error = BonsaiClient::new(&url)
            .chat_stream(&[ChatMessage::user("hi")], &tiny_params(), &cancel, &mut |_| {})
            .expect_err("400");
        assert_eq!(error.to_string(), "Bonsai error 400: Failed to load image or audio file");
        server.join().expect("server");
    }

    #[test]
    fn mock_chat_stream_cancel_while_server_is_silent() {
        // The server reads the request and then sends nothing (prompt processing), waiting for
        // the client to hang up. It reports whether and how fast it saw the connection close.
        let (url, server) = serve_once(|mut stream, _request| {
            stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
            let mut buf = [0u8; 64];
            let closed = match stream.read(&mut buf) {
                Ok(0) => true,
                Ok(_) => false,
                Err(err) => matches!(err.kind(), io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted),
            };
            (closed, Instant::now())
        });
        let client = BonsaiClient::new(&url);
        let cancel = AtomicBool::new(false);
        let (result, cancelled_at, returned_at) = thread::scope(|scope| {
            let setter = scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::Relaxed);
                Instant::now()
            });
            let result = client.chat_stream(&[ChatMessage::user("hi")], &tiny_params(), &cancel, &mut |_| {});
            let returned_at = Instant::now();
            (result, setter.join().expect("setter"), returned_at)
        });
        let result = result.expect("cancelled result");
        assert!(result.cancelled);
        assert_eq!(result.finish_reason, "cancelled");
        let latency = returned_at.saturating_duration_since(cancelled_at);
        assert!(latency < Duration::from_millis(200), "returned {latency:?} after the cancel");
        let (closed, seen_at) = server.join().expect("server");
        assert!(closed, "server did not see the connection close");
        assert!(seen_at.saturating_duration_since(cancelled_at) < Duration::from_secs(1));
    }

    #[test]
    fn nothing_listening_is_down_and_an_error() {
        // A port that was just free. Windows takes ~2 s to refuse a localhost connect.
        let port = TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port();
        let client = BonsaiClient::new(&format!("http://127.0.0.1:{port}"));
        assert!(matches!(client.health(), Health::Down(_)));
        let cancel = AtomicBool::new(false);
        let error = client
            .chat_stream(&[ChatMessage::user("hi")], &tiny_params(), &cancel, &mut |_| {})
            .expect_err("no server");
        assert!(error.to_string().starts_with("cannot reach Bonsai at http://127.0.0.1:"), "{error}");
    }

    #[test]
    fn cancel_before_start_does_not_connect() {
        // Nothing listens on this URL; a set cancel flag must return at once anyway.
        let cancel = AtomicBool::new(true);
        let result = BonsaiClient::new("http://127.0.0.1:9")
            .chat_stream(&[ChatMessage::user("hi")], &tiny_params(), &cancel, &mut |_| {})
            .expect("cancelled");
        assert!(result.cancelled);
        assert_eq!(result.finish_reason, "cancelled");
    }

    // ----- live server (run with `cargo test --lib client:: -- --ignored`) -------------------

    mod live {
        use super::*;

        const LIVE_URL: &str = "http://127.0.0.1:8080";

        fn live_params(max_tokens: u32) -> GenParams {
            GenParams { thinking: ThinkingMode::Off, temperature: 0.4, max_tokens, ..GenParams::prompt_writer() }
        }

        #[test]
        #[ignore = "needs Bonsai running at 127.0.0.1:8080"]
        fn live_health() {
            assert_eq!(BonsaiClient::new(LIVE_URL).health(), Health::Ready);
        }

        #[test]
        #[ignore = "needs Bonsai running at 127.0.0.1:8080"]
        fn live_props_and_slots() {
            let client = BonsaiClient::new(LIVE_URL);
            let props = client.props().expect("props");
            println!("{props:#?}");
            assert!(props.model_name.ends_with(".gguf"), "{props:?}");
            assert!(props.model_path.ends_with(&props.model_name));
            assert!(props.n_ctx >= 1024);
            assert!(props.total_slots >= 1);
            assert!(!props.build_info.is_empty());
            assert!(!props.model_ftype.is_empty());
            let busy = client.slots_busy().expect("slots");
            println!("slots busy: {busy}");
        }

        #[test]
        #[ignore = "needs Bonsai running at 127.0.0.1:8080"]
        fn live_tokenize_count() {
            let count = BonsaiClient::new(LIVE_URL).tokenize_count("Hello world, my friend.").expect("tokenize");
            assert!((3..=12).contains(&count), "{count}");
        }

        #[test]
        #[ignore = "needs Bonsai running at 127.0.0.1:8080"]
        fn live_chat_stream_thinking_off() {
            let client = BonsaiClient::new(LIVE_URL);
            let cancel = AtomicBool::new(false);
            let mut streamed = String::new();
            let mut pieces = 0;
            let result = client
                .chat_stream(
                    &[ChatMessage::user("Say hello to my friend in five words.")],
                    &live_params(20),
                    &cancel,
                    &mut |event| {
                        pieces += 1;
                        if let StreamEvent::Content(text) = event {
                            streamed.push_str(&text);
                        }
                    },
                )
                .expect("stream");
            println!("{result:#?}");
            assert!(!result.content.trim().is_empty());
            assert_eq!(streamed, result.content);
            assert!(pieces >= 2, "{pieces} pieces");
            assert!(result.reasoning.is_empty(), "thinking was off: {:?}", result.reasoning);
            assert!(["stop", "length"].contains(&result.finish_reason.as_str()), "{}", result.finish_reason);
            let usage = result.usage.expect("usage");
            assert!(usage.completion_tokens >= 1 && usage.completion_tokens <= 20, "{usage:?}");
            let timings = result.timings.expect("timings");
            assert!(timings.predicted_n >= 1 && timings.predicted_per_second > 0.0, "{timings:?}");
            assert!(!result.cancelled);
            assert!(result.elapsed_ms > 0);
        }

        #[test]
        #[ignore = "needs Bonsai running at 127.0.0.1:8080"]
        fn live_chat_non_streaming() {
            let result = BonsaiClient::new(LIVE_URL)
                .chat(&[ChatMessage::user("Reply with one word: hello.")], &live_params(10))
                .expect("chat");
            println!("{result:#?}");
            assert!(!result.content.trim().is_empty());
            assert!(result.usage.is_some() && result.timings.is_some());
        }

        #[test]
        #[ignore = "needs Bonsai running at 127.0.0.1:8080"]
        fn live_cancel_after_first_piece() {
            let client = BonsaiClient::new(LIVE_URL);
            let cancel = AtomicBool::new(false);
            let mut cancelled_at: Option<Instant> = None;
            let mut pieces = 0;
            let result = client
                .chat_stream(
                    &[ChatMessage::user("Count from one to fifty in words, separated by commas.")],
                    &live_params(60),
                    &cancel,
                    &mut |_| {
                        pieces += 1;
                        if cancelled_at.is_none() {
                            cancelled_at = Some(Instant::now());
                            cancel.store(true, Ordering::Relaxed);
                        }
                    },
                )
                .expect("cancelled stream");
            let after_cancel = cancelled_at.expect("a first piece arrived").elapsed();
            println!("returned {after_cancel:?} after the cancel: {result:#?}");
            assert!(result.cancelled);
            assert_eq!(result.finish_reason, "cancelled");
            assert_eq!(pieces, 1);
            assert!(!result.content.is_empty());
            assert!(after_cancel < Duration::from_secs(1), "{after_cancel:?}");
            // The server keeps answering after the dropped connection.
            assert_eq!(client.health(), Health::Ready);
        }
    }
}
