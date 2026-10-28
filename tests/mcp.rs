//! Integration tests of `bh3 mcp`: they run the real binary as an MCP server over stdin/stdout.
//!
//! Every server gets its own empty config folder (`BH3_CONFIG_DIR`) whose config.json has
//! `auto_start: false`, and talks to `http://127.0.0.1:9`, where nothing listens: no test can
//! reach or start Bonsai. The only process a test ends is its own `bh3 mcp`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BH3: &str = env!("CARGO_BIN_EXE_bh3");
/// Nothing listens here (TCP "discard" port).
const DEAD_URL: &str = "http://127.0.0.1:9";
/// Longest wait for one answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// The 12 tools of `tools/list`.
const TOOLS: [&str; 12] = [
    "bonsai_status",
    "write_prompt",
    "fix_prompt",
    "lint_prompt",
    "clip_length",
    "get_guide",
    "list_templates",
    "list_history",
    "get_history_entry",
    "start_bonsai",
    "stop_bonsai",
    "chat",
];

/// A running `bh3 mcp` with its own config folder.
struct McpServer {
    child: Child,
    stdin: Option<ChildStdin>,
    /// stdout lines, read by a thread.
    lines: Receiver<String>,
    /// stderr so far (shown when an answer is missing).
    stderr: Arc<Mutex<String>>,
    dir: PathBuf,
    next_id: u64,
}

impl McpServer {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bh3-mcp-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), json!({ "auto_start": false }).to_string()).unwrap();
        let mut child = Command::new(BH3)
            .args(["--url", DEAD_URL, "mcp"])
            .env("BH3_CONFIG_DIR", &dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("starting bh3 mcp");
        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let pipe = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                let Ok(line) = line else { break };
                sink.lock().unwrap().push_str(&format!("{line}\n"));
            }
        });
        let stdin = child.stdin.take();
        Self { child, stdin, lines, stderr, dir, next_id: 0 }
    }

    fn history(&self) -> PathBuf {
        self.dir.join("history")
    }

    fn send_line(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(text.as_bytes()).and_then(|()| stdin.write_all(b"\n")).and_then(|()| stdin.flush()).unwrap();
    }

    fn send(&mut self, message: &Value) {
        self.send_line(&message.to_string());
    }

    /// The next stdout line as JSON.
    fn recv(&self) -> Value {
        let line = self.lines.recv_timeout(ANSWER_TIMEOUT).unwrap_or_else(|e| {
            panic!("no answer within {ANSWER_TIMEOUT:?} ({e}); stderr:\n{}", self.stderr.lock().unwrap())
        });
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout line is not one JSON message ({e}): {line}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let reply = self.recv();
        assert_eq!((&reply["jsonrpc"], &reply["id"]), (&json!("2.0"), &json!(id)), "{reply}");
        reply
    }

    /// `tools/call`: the result (a JSON-RPC error fails the test).
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let reply = self.request("tools/call", json!({ "name": tool, "arguments": arguments }));
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }

    /// Close stdin and wait at most `limit` for the exit code.
    fn close(&mut self, limit: Duration) -> Option<i32> {
        self.stdin = None;
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stdin = None;
        // Our own bh3 process only (it has exited already when the test passed).
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The text content of a tool result.
fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text content: {result}"))
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)).unwrap()
}

#[test]
fn mcp_protocol() {
    let mut server = McpServer::start("protocol");
    let init = server.request(
        "initialize",
        json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }),
    );
    let result = &init["result"];
    assert_eq!(result["protocolVersion"], "2025-03-26", "{init}");
    assert_eq!(result["serverInfo"]["name"], "bonsai-h3-writer");
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["instructions"].as_str().unwrap().contains("stop_bonsai"));

    // A notification gets no answer: the next line is the answer to the ping after it.
    server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    assert_eq!(server.request("ping", json!({}))["result"], json!({}));

    let list = server.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOLS);
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(tool["description"].as_str().is_some_and(|d| d.len() > 20), "{tool}");
    }

    assert_eq!(server.request("resources/list", json!({}))["error"]["code"], -32601);
    let unknown = server.request("tools/call", json!({ "name": "no_such_tool", "arguments": {} }));
    assert_eq!(unknown["error"]["code"], -32602);
    server.send_line("{this is not json");
    let parse = server.recv();
    assert_eq!((&parse["error"]["code"], &parse["id"]), (&json!(-32700), &Value::Null), "{parse}");
    server.send_line(r#"[{"jsonrpc":"2.0","id":"a","method":"ping"},{"jsonrpc":"2.0","id":"b","method":"ping"}]"#);
    let batch = server.recv();
    let ids: Vec<&Value> =
        batch.as_array().expect("a batch is answered with an array").iter().map(|r| &r["id"]).collect();
    assert_eq!(ids, [&json!("a"), &json!("b")]);

    assert_eq!(server.close(Duration::from_secs(5)), Some(0), "exit 0 when stdin closes");
}

#[test]
fn mcp_tools_without_bonsai() {
    let mut server = McpServer::start("tools");
    server.request("initialize", json!({ "protocolVersion": "2025-06-18" }));

    let good = server.call(
        "lint_prompt",
        json!({ "prompt": fixture("good_r2v_e1.txt"), "mode": "r2v", "duration_s": 3, "refs": 1, "first_frame_pinned": false }),
    );
    assert_eq!((&good["isError"], &good["structuredContent"]["ok"]), (&json!(false), &json!(true)), "{good}");
    let bad =
        server.call("lint_prompt", json!({ "prompt": fixture("bad_r2v_markdown_fake_shots.txt"), "duration_s": 15 }));
    assert!(bad["structuredContent"]["errors"].as_u64().unwrap() > 0, "{bad}");
    assert_eq!(bad["structuredContent"]["mode"], "r2v", "detected from the text");

    let length = server.call("clip_length", json!({ "seconds": 6 }));
    assert_eq!(length["structuredContent"]["real_text"], "6.58", "{length}");
    let guide = server.call("get_guide", json!({ "mode": "r2v" }));
    assert!(text(&guide).len() > 200, "{guide}");
    assert!(guide["structuredContent"]["official_guide"].as_str().unwrap().ends_with("_ref_en.md"));
    let templates = server.call("list_templates", json!({}));
    assert!(templates["structuredContent"]["templates"].as_array().is_some_and(|t| t.len() >= 10), "{templates}");
    let f2v = server.call("list_templates", json!({ "mode": "f2v" }));
    assert!(f2v["structuredContent"]["templates"].as_array().unwrap().iter().all(|t| t["mode"] == "f2v"));

    let status = server.call("bonsai_status", json!({}));
    assert_eq!(status["isError"], false, "{status}");
    assert_ne!(status["structuredContent"]["state"], "ready");
    assert_eq!(status["structuredContent"]["url"], DEAD_URL);
    assert_eq!(status["structuredContent"]["auto_start"], false);

    // Bonsai is down and auto-start is off: a failed call, nothing is started.
    let write =
        server.call("write_prompt", json!({ "mode": "r2v", "request": "Ref 1: a woman. 6 s, one take: she waves." }));
    assert_eq!(write["isError"], true, "{write}");
    assert!(text(&write).contains("not running"), "{write}");
    assert!(text(&write).contains("start_bonsai"), "the hint names the tool: {write}");
    assert_eq!(write["structuredContent"]["kind"], "unavailable");
    let wrong = server.call("write_prompt", json!({ "mode": "t2v", "request": "x" }));
    assert_eq!((&wrong["isError"], &wrong["structuredContent"]["kind"]), (&json!(true), &json!("usage")), "{wrong}");

    assert_eq!(server.call("list_history", json!({}))["structuredContent"]["entries"], json!([]));
    assert_eq!(server.call("get_history_entry", json!({ "id": "../x" }))["isError"], true);
    let nothing = server.call("fix_prompt", json!({ "fix": "Make it longer." }));
    assert!(text(&nothing).contains("no saved prompts"), "{nothing}");

    // A saved entry: listed, shown with a fresh check, and fixing it needs Bonsai.
    let id = "20261028-101500-000";
    std::fs::create_dir_all(server.history()).unwrap();
    let entry = json!({
        "id": id,
        "created": "2026-10-28T10:15:00.000+05:45",
        "mode": "r2v",
        "title": "Ref 1: a woman",
        "request": "Ref 1: a woman. 3 s, one take: she turns.",
        "output": fixture("good_r2v_e1.txt"),
        "lint_score": 100,
        "lint_errors": 0,
    });
    std::fs::write(server.history().join(format!("{id}.json")), entry.to_string()).unwrap();
    let history = server.call("list_history", json!({ "limit": 5 }));
    assert_eq!(history["structuredContent"]["entries"][0]["id"], id, "{history}");
    let shown = server.call("get_history_entry", json!({ "id": "last" }));
    assert_eq!(shown["structuredContent"]["id"], id);
    assert_eq!(shown["structuredContent"]["lint"]["ok"], true, "{shown}");
    let fix = server.call("fix_prompt", json!({ "history_id": id, "fix": "Make it longer.", "auto_start": false }));
    assert!(text(&fix).contains("not running"), "{fix}");

    assert_eq!(server.close(Duration::from_secs(5)), Some(0));
}
