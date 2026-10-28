//! Integration tests of the `bh3` command line: they run the real binary.
//!
//! Every test gets its own empty config folder (`BH3_CONFIG_DIR`) whose config.json has
//! `auto_start: false`, so the user's settings and history are never touched and no test can
//! start Bonsai. Commands that would reach Bonsai talk to `http://127.0.0.1:9`, where nothing
//! listens. Tests that need the real server are ignored: `cargo test --test cli -- --ignored`
//! (Bonsai must already be running on http://127.0.0.1:8080; thinking off, ~20 s per prompt).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const BH3: &str = env!("CARGO_BIN_EXE_bh3");
/// Nothing listens here (TCP "discard" port).
const DEAD_URL: &str = "http://127.0.0.1:9";

/// An empty config folder for one test, removed afterwards.
struct ConfigDir(PathBuf);

impl ConfigDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bh3-cli-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), json!({ "auto_start": false }).to_string()).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn history(&self) -> PathBuf {
        self.0.join("history")
    }

    /// Run bh3 with this config folder and an empty stdin.
    fn run(&self, args: &[&str]) -> Run {
        run_bh3(self.path(), args, None)
    }

    /// Run bh3 with `input` on stdin.
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Run {
        run_bh3(self.path(), args, Some(input))
    }
}

impl Drop for ConfigDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Exit code and output of one bh3 run.
#[derive(Debug)]
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    /// stdout as JSON (panics with the output when it is not).
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}\nstderr:\n{}", self.stdout, self.stderr))
    }

    #[track_caller]
    fn expect(&self, code: i32) -> &Self {
        assert_eq!(self.code, code, "exit code\nstdout:\n{}\nstderr:\n{}", self.stdout, self.stderr);
        self
    }
}

fn run_bh3(config_dir: &Path, args: &[&str], input: Option<&str>) -> Run {
    let mut cmd = Command::new(BH3);
    cmd.args(args)
        .env("BH3_CONFIG_DIR", config_dir)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("starting bh3");
    if let Some(text) = input {
        let mut stdin = child.stdin.take().expect("stdin pipe");
        stdin.write_all(text.as_bytes()).expect("writing stdin");
    }
    let out = child.wait_with_output().expect("waiting for bh3");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")
}

/// One line of `tests/fixtures/README.tsv`: file name and `key=value` facts.
struct Fixture {
    file: String,
    facts: Vec<(String, String)>,
}

impl Fixture {
    fn fact(&self, key: &str) -> &str {
        self.facts.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or_else(|| panic!("{key}"))
    }

    fn path(&self) -> String {
        fixtures().join(&self.file).to_string_lossy().into_owned()
    }

    /// `lint --file <fixture> --mode .. --duration .. --refs .. --pinned=.. --last-frame=.. --json`.
    fn lint_args(&self) -> Vec<String> {
        vec![
            "lint".into(),
            "--file".into(),
            self.path(),
            "--mode".into(),
            self.fact("mode").into(),
            "--duration".into(),
            self.fact("duration").into(),
            "--refs".into(),
            self.fact("refs").into(),
            format!("--pinned={}", self.fact("pinned")),
            format!("--last-frame={}", self.fact("last_frame")),
            "--json".into(),
        ]
    }
}

fn fixture_list() -> Vec<Fixture> {
    let index = std::fs::read_to_string(fixtures().join("README.tsv")).unwrap();
    index
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let file = fields.next().unwrap().to_string();
            let facts = fields
                .filter_map(|f| f.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect();
            Fixture { file, facts }
        })
        .collect()
}

fn error_codes(report: &Value) -> Vec<String> {
    report["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["severity"] == "error")
        .map(|i| i["code"].as_str().unwrap().to_string())
        .collect()
}

/// A history entry file as the app writes it (`history\<id>.json`).
fn write_entry(dir: &ConfigDir, id: &str, mode: &str, request: &str, output: &str, extra: Value) {
    let mut entry = json!({
        "id": id,
        "created": format!("{}-{}-{}T{}:{}:00.000+05:45", &id[0..4], &id[4..6], &id[6..8], &id[9..11], &id[11..13]),
        "mode": mode,
        "title": request.lines().next().unwrap_or(""),
        "request": request,
        "output": output,
        "lint_score": 92,
        "lint_errors": 0,
    });
    if let (Some(entry), Some(extra)) = (entry.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            entry.insert(k.clone(), v.clone());
        }
    }
    std::fs::create_dir_all(dir.history()).unwrap();
    std::fs::write(dir.history().join(format!("{id}.json")), serde_json::to_string_pretty(&entry).unwrap()).unwrap();
}

// ---------------------------------------------------------------------------------------------
// lint
// ---------------------------------------------------------------------------------------------

#[test]
fn lint_passes_every_good_fixture() {
    let dir = ConfigDir::new("lint-good");
    let good: Vec<Fixture> = fixture_list().into_iter().filter(|f| f.file.starts_with("good_")).collect();
    assert!(good.len() >= 10, "only {} good fixtures", good.len());
    for fixture in &good {
        let args = fixture.lint_args();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let run = dir.run(&args);
        assert_eq!(run.code, 0, "{}: {}", fixture.file, run.stdout);
        let report = run.json();
        assert_eq!(report["ok"], true, "{}", fixture.file);
        assert_eq!(report["errors"], 0, "{}", fixture.file);
        assert_eq!(report["mode"], fixture.fact("mode"), "{}", fixture.file);
        assert_eq!(report["context"]["ref_count"], fixture.fact("refs").parse::<u64>().unwrap(), "{}", fixture.file);
    }
}

#[test]
fn lint_reports_the_errors_of_the_bad_fixture() {
    let dir = ConfigDir::new("lint-bad");
    let fixture = fixture_list().into_iter().find(|f| f.file.starts_with("bad_")).unwrap();
    let args = fixture.lint_args();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = dir.run(&args);
    run.expect(1);
    let report = run.json();
    assert_eq!(report["ok"], false);
    let codes = error_codes(&report);
    for expected in ["markdown-headings", "shot-time-format", "single-take-with-shots"] {
        assert!(codes.iter().any(|c| c == expected), "{expected} missing in {codes:?}");
    }
    assert_eq!(report["errors"].as_u64().unwrap() as usize, codes.len());
    assert!(report["score"].as_u64().unwrap() < 50);
    assert_eq!(report["context"]["duration_s"], 15.0);

    // Plain text: the issues on stdout, exit 1.
    let run = dir.run(&["lint", "--file", &fixture.path(), "--duration", "15"]);
    run.expect(1);
    assert!(run.stdout.contains("error markdown-headings"), "{}", run.stdout);
    assert!(run.stdout.contains("Not ready"), "{}", run.stdout);
}

#[test]
fn lint_reads_stdin_and_detects_the_mode() {
    let dir = ConfigDir::new("lint-stdin");
    let f2v = std::fs::read_to_string(fixtures().join("good_f2v_e7.txt")).unwrap();
    let run = dir.run_with_stdin(&["lint", "--json"], &f2v);
    run.expect(0);
    assert_eq!(run.json()["mode"], "f2v");

    // CRLF and a BOM are fine too.
    let crlf = format!("\u{feff}{}", f2v.replace('\n', "\r\n"));
    dir.run_with_stdin(&["lint", "--duration", "3", "--last-frame=false"], &crlf).expect(0);

    // An r2v prompt checked as f2v is the wrong mode.
    let r2v = std::fs::read_to_string(fixtures().join("good_r2v_e1.txt")).unwrap();
    let run = dir.run_with_stdin(&["lint", "--mode", "f2v", "--json"], &r2v);
    run.expect(1);
    assert_eq!(error_codes(&run.json()), vec!["wrong-mode"]);
}

#[test]
fn lint_without_input_is_a_usage_error() {
    let dir = ConfigDir::new("lint-empty");
    let run = dir.run(&["lint"]);
    run.expect(2);
    assert!(run.stderr.contains("stdin"), "{}", run.stderr);
    dir.run(&["lint", "--file", "does-not-exist.txt"]).expect(1);
    dir.run(&["lint", "--refs", "12"]).expect(2);
}

// ---------------------------------------------------------------------------------------------
// lengths, library, version, agent guide
// ---------------------------------------------------------------------------------------------

#[test]
fn lengths_table_and_json() {
    let dir = ConfigDir::new("lengths");
    let run = dir.run(&["lengths"]);
    run.expect(0);
    for real in ["3.04 s", "6.58 s", "10.13 s", "15.08 s"] {
        assert!(run.stdout.contains(real), "{real} missing:\n{}", run.stdout);
    }
    let rows = dir.run(&["lengths", "6", "10", "--json"]).json();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["seconds"], 6.0);
    assert_eq!(rows[0]["frames"], 158);
    assert_eq!(rows[0]["real"], "6.58");
    assert_eq!(rows[0]["real_s"], 6.58);
    assert_eq!(rows[0]["max_cuts"], 2);
    assert_eq!(rows[0]["max_megapixels"], 0.5);
    assert_eq!(rows[1]["real"], "10.13");
    dir.run(&["lengths", "0"]).expect(2);
}

#[test]
fn library_commands_print_the_builtin_texts() {
    let dir = ConfigDir::new("library");
    let templates = dir.run(&["templates", "--json"]).json();
    let templates = templates.as_array().unwrap();
    assert!(templates.len() >= 10);
    let first_frame = templates.iter().find(|t| t["id"] == "first-frame-only").expect("first-frame-only");
    assert_eq!(first_frame["mode"], "f2v");
    let number = first_frame["number"].as_u64().unwrap().to_string();

    let f2v = dir.run(&["templates", "--mode", "f2v", "--json"]).json();
    assert!(f2v.as_array().unwrap().iter().all(|t| t["mode"] == "f2v"));
    let table = dir.run(&["templates"]);
    assert!(table.stdout.contains("first-frame-only"), "{}", table.stdout);

    let run = dir.run(&["template", "first-frame-only"]);
    run.expect(0);
    assert!(run.stdout.starts_with("Picture 1:"), "{}", run.stdout);
    assert_eq!(dir.run(&["template", &number]).stdout, run.stdout, "by number");
    dir.run(&["template", "no-such-template"]).expect(1);

    let fixes = dir.run(&["fixes", "--json"]).json();
    let fixes: Vec<&str> = fixes.as_array().unwrap().iter().map(|f| f.as_str().unwrap()).collect();
    assert!(fixes.contains(&"Remove the music (non_diegetic_music: N/A)."), "{fixes:?}");
    assert!(!fixes.iter().any(|f| f.starts_with("FOLLOW-UP FIXES")), "the header is not a fix");

    let run = dir.run(&["system-prompt", "r2v"]);
    run.expect(0);
    assert!(run.stdout.starts_with("You write MiniMax H3 reference-to-video prompts"), "{}", run.stdout);
    let system = dir.run(&["system-prompt", "f2v", "--json"]).json();
    assert_eq!(system["item"], "system-f2v");
    assert_eq!(system["overridden"], false);
    assert!(system["text"].as_str().unwrap().contains("I2VA"));

    let guide = dir.run(&["guide", "f2v"]);
    guide.expect(0);
    assert!(guide.stdout.contains("FL2VA"), "{}", guide.stdout);
    let guide = dir.run(&["guide", "r2v", "--json"]).json();
    assert!(guide["official_guide"].as_str().unwrap().ends_with("VIDEO_PROMPT_WRITING_GUIDE_ref_en.md"));
    dir.run(&["guide", "t2v"]).expect(2);
}

#[test]
fn library_commands_use_the_users_edited_texts() {
    let dir = ConfigDir::new("library-override");
    let library = dir.path().join("library");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(library.join("fixes.txt"), "Make it rain.\nAdd a red umbrella.\n").unwrap();
    std::fs::write(library.join("system_r2v.txt"), "\u{feff}My own system prompt.\r\n").unwrap();
    assert_eq!(dir.run(&["fixes", "--json"]).json(), json!(["Make it rain.", "Add a red umbrella."]));
    let system = dir.run(&["system-prompt", "r2v", "--json"]).json();
    assert_eq!(system["overridden"], true);
    assert_eq!(system["text"].as_str().unwrap().trim(), "My own system prompt.");
}

#[test]
fn version_and_agent_guide() {
    let dir = ConfigDir::new("version");
    let run = dir.run(&["version"]);
    run.expect(0);
    assert!(run.stdout.contains(env!("CARGO_PKG_VERSION")), "{}", run.stdout);
    assert_eq!(dir.run(&["version", "--json"]).json()["version"], env!("CARGO_PKG_VERSION"));
    let flag = dir.run(&["--version"]);
    flag.expect(0);
    assert_eq!(flag.stdout.trim(), format!("bh3 {}", env!("CARGO_PKG_VERSION")));

    let guide = dir.run(&["agent-guide"]);
    guide.expect(0);
    for part in ["bh3 status --json", "--stop-after", "lint.ok", "bh3 fix --last"] {
        assert!(guide.stdout.contains(part), "{part} missing in the agent guide");
    }
    let help = dir.run(&["--help"]);
    help.expect(0);
    assert!(help.stdout.contains("Usage: bh3"), "{}", help.stdout);
    dir.run(&["write", "--help"]).expect(0);
}

// ---------------------------------------------------------------------------------------------
// config, install, logs
// ---------------------------------------------------------------------------------------------

#[test]
fn config_set_get_show_round_trip() {
    let dir = ConfigDir::new("config");
    let run = dir.run(&["config", "path"]);
    run.expect(0);
    assert_eq!(PathBuf::from(run.stdout.trim()), dir.path().join("config.json"));

    dir.run(&["config", "set", "writer.temperature", "0.3"]).expect(0);
    assert_eq!(dir.run(&["config", "get", "writer.temperature"]).stdout.trim(), "0.3");
    dir.run(&["config", "set", "Writer_Params.Thinking", "low"]).expect(0);
    dir.run(&["config", "set", "auto_stop_idle_minutes", "25"]).expect(0);
    dir.run(&["config", "set", "chat.seed", "-1"]).expect(0);
    let set = dir.run(&["config", "set", "server_url", "http://127.0.0.1:8081", "--json"]).json();
    assert_eq!(set, json!({ "key": "server_url", "value": "http://127.0.0.1:8081" }));

    let cfg = dir.run(&["config", "show", "--json"]).json();
    assert_eq!(cfg["writer_params"]["temperature"], 0.3);
    assert_eq!(cfg["writer_params"]["thinking"], "low");
    assert_eq!(cfg["chat_params"]["seed"], -1);
    assert_eq!(cfg["auto_stop_idle_minutes"], 25);
    assert_eq!(cfg["auto_start"], false, "the file's other settings are kept");
    assert_eq!(cfg["server_url"], "http://127.0.0.1:8081");
    let show = dir.run(&["config", "show"]);
    assert!(show.stdout.contains("writer.temperature      = 0.3"), "{}", show.stdout);

    // The file on disk is what the app reads.
    let file: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(file["writer_params"]["thinking"], "low");

    dir.run(&["config", "set", "chat.seed", "none"]).expect(0);
    assert_eq!(dir.run(&["config", "get", "chat.seed", "--json"]).json()["value"], Value::Null);
    dir.run(&["config", "get", "no_such_key"]).expect(2);
    dir.run(&["config", "set", "writer.temperature", "5"]).expect(2);
    dir.run(&["config", "set", "auto_start", "sometimes"]).expect(2);
    dir.run(&["config", "set", "writer.thinking", "extreme"]).expect(2);

    // A broken config file is reported, not overwritten.
    std::fs::write(dir.path().join("config.json"), "{ not json").unwrap();
    dir.run(&["config", "set", "auto_start", "true"]).expect(1);
    assert_eq!(std::fs::read_to_string(dir.path().join("config.json")).unwrap(), "{ not json");
}

#[test]
fn install_set_checks_the_folder() {
    let dir = ConfigDir::new("install");
    let not_bonsai = dir.path().join("empty-folder");
    std::fs::create_dir_all(&not_bonsai).unwrap();
    let run = dir.run(&["install", "set", not_bonsai.to_str().unwrap()]);
    run.expect(1);
    assert!(run.stderr.contains("not a Bonsai-demo folder"), "{}", run.stderr);

    // A fake Bonsai-demo folder (empty files are enough for detection).
    let fake = dir.path().join("Bonsai-demo");
    for part in ["scripts/start_llama_server.ps1", "bin/cuda/llama-server.exe", "models/bonsai2-gguf/27B/m-PTQ1_0.gguf"]
    {
        let path = fake.join(part);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }
    let install = dir.run(&["install", "set", fake.to_str().unwrap(), "--json"]).json();
    assert_eq!(install["usable"], true, "{install}");
    assert_eq!(install["backend"], "CUDA");
    assert_eq!(PathBuf::from(dir.run(&["config", "get", "bonsai_dir"]).stdout.trim()), fake);
    let show = dir.run(&["install", "show", "--json"]);
    show.expect(0);
    let show = show.json();
    assert_eq!(show["source"], "config");
    assert_eq!(PathBuf::from(show["root"].as_str().unwrap()), fake);
    // Not usable: the report is printed, exit 1.
    let show = dir.run(&["--bonsai-dir", not_bonsai.to_str().unwrap(), "install", "show", "--json"]);
    show.expect(1);
    let show = show.json();
    assert_eq!(show["source"], "option");
    assert_eq!(show["usable"], false);

    let detect = dir.run(&["install", "detect", "--json"]);
    detect.expect(0);
    assert!(detect.json()["installs"].is_array());
}

#[test]
fn logs_print_the_end_of_the_server_log() {
    let dir = ConfigDir::new("logs");
    let none = dir.run(&["logs", "--json"]).json();
    assert_eq!(none["exists"], false);
    assert_eq!(none["lines"], json!([]));
    std::fs::write(dir.path().join("server.log"), "=== start ===\nline 1\nline 2\nmain: server is listening\n")
        .unwrap();
    let run = dir.run(&["logs", "--lines", "2"]);
    run.expect(0);
    assert_eq!(run.stdout.trim_end().lines().collect::<Vec<_>>(), vec!["line 2", "main: server is listening"]);
    assert_eq!(dir.run(&["logs", "--json"]).json()["lines"].as_array().unwrap().len(), 4);
}

// ---------------------------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------------------------

#[test]
fn history_list_show_export_delete() {
    let dir = ConfigDir::new("history");
    let prompt = std::fs::read_to_string(fixtures().join("good_r2v_e2.txt")).unwrap();
    let f2v = std::fs::read_to_string(fixtures().join("good_f2v_e7.txt")).unwrap();
    write_entry(&dir, "20261027-090000-000", "r2v", "Ref 1: a woman. Ref 2: a man. 3 s, one take.", &prompt, json!({}));
    write_entry(
        &dir,
        "20261028-101500-000",
        "r2v",
        "Ref 1: a woman. Ref 2: a man. 3 s, one take.\n\nFollow-up fix: Make it longer.",
        &prompt,
        json!({ "parent_id": "20261027-090000-000", "starred": true, "duration_s": 3 }),
    );
    write_entry(&dir, "20261026-080000-000", "f2v", "Picture 1: a man at a table. 3 s.", &f2v, json!({}));

    let list = dir.run(&["history", "list", "--json"]).json();
    let ids: Vec<&str> = list.as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["20261028-101500-000", "20261027-090000-000", "20261026-080000-000"]);
    assert_eq!(list[0]["fix"], "Make it longer.");
    assert_eq!(list[0]["parent_id"], "20261027-090000-000");
    assert_eq!(list[0]["date"], "2026-10-28 10:15");
    assert_eq!(dir.run(&["history", "list", "--limit", "1", "--json"]).json().as_array().unwrap().len(), 1);
    let f2v_only = dir.run(&["history", "list", "--mode", "f2v", "--json"]).json();
    assert_eq!(f2v_only.as_array().unwrap().len(), 1);
    assert_eq!(dir.run(&["history", "list", "--starred", "--json"]).json().as_array().unwrap().len(), 1);
    assert_eq!(dir.run(&["history", "list", "--search", "table", "--json"]).json()[0]["mode"], "f2v");
    let table = dir.run(&["history", "list"]);
    assert!(table.stdout.contains("20261027-090000-000  2026-10-27 09:00  r2v"), "{}", table.stdout);

    // show: the newest entry, its parts and the whole JSON.
    let run = dir.run(&["history", "show", "last", "--field", "prompt"]);
    run.expect(0);
    assert_eq!(run.stdout.trim_end(), prompt.trim_end());
    assert_eq!(
        dir.run(&["history", "show", "last", "--field", "request"]).stdout.trim_end(),
        "Ref 1: a woman. Ref 2: a man. 3 s, one take."
    );
    assert_eq!(dir.run(&["history", "show", "last", "--field", "fix"]).stdout.trim_end(), "Make it longer.");
    let full = dir.run(&["history", "show", "20261026-080000-000", "--json"]).json();
    assert_eq!(full["mode"], "f2v");
    assert_eq!(full["request"], "Picture 1: a man at a table. 3 s.");
    let view = dir.run(&["history", "show", "20261028-101500-000"]);
    assert!(view.stdout.contains("--- fix ---\nMake it longer."), "{}", view.stdout);
    dir.run(&["history", "show", "20261231-000000-000"]).expect(1);

    // export: UTF-8 with BOM and CRLF.
    let file = dir.path().join("out").join("prompt.txt");
    dir.run(&["history", "export", "20261027-090000-000", file.to_str().unwrap()]).expect(0);
    let bytes = std::fs::read(&file).unwrap();
    assert!(bytes.starts_with(b"\xEF\xBB\xBFsubject_definitions:\r\n"), "{:?}", &bytes[..30]);

    // delete
    dir.run(&["history", "delete", "last"]).expect(2);
    let deleted = dir.run(&["history", "delete", "20261027-090000-000", "--json"]).json();
    assert_eq!(deleted, json!({ "deleted": "20261027-090000-000" }));
    assert_eq!(dir.run(&["history", "list", "--json"]).json().as_array().unwrap().len(), 2);
    dir.run(&["history", "delete", "20261027-090000-000"]).expect(1);
    dir.run(&["history", "delete", "../config"]).expect(1);
}

#[test]
fn history_of_an_empty_folder() {
    let dir = ConfigDir::new("history-empty");
    assert_eq!(dir.run(&["history", "list", "--json"]).json(), json!([]));
    dir.run(&["history", "list"]).expect(0);
    dir.run(&["history", "show", "last"]).expect(1);
    dir.run(&["fix", "--last", "--fix", "Make it longer.", "--url", DEAD_URL, "--no-auto-start"]).expect(1);
}

// ---------------------------------------------------------------------------------------------
// usage errors and Bonsai unavailable
// ---------------------------------------------------------------------------------------------

#[test]
fn usage_errors_exit_2() {
    let dir = ConfigDir::new("usage");
    let cases: [&[&str]; 13] = [
        &[],
        &["bogus"],
        &["write", "--mode", "r2v"],
        &["write", "--request", "Ref 1: a woman."],
        &["write", "--mode", "r2v", "--request", "x", "--stdin"],
        &["write", "--mode", "r2v", "--request", "x", "--thinking", "extreme"],
        &["write", "--mode", "r2v", "--request", "x", "--temperature", "9"],
        &["lint", "--mode", "t2v"],
        &["lint", "--pinned=maybe"],
        &["history", "show"],
        &["config", "set", "auto_start"],
        &["fix", "--fix", "Make it longer."],
        &["chat"],
    ];
    for args in cases {
        let run = dir.run(args);
        assert_eq!(run.code, 2, "{args:?}\nstdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
        assert!(!run.stderr.is_empty(), "{args:?}: no message");
    }

    // With --json the usage error is JSON on stdout as well.
    let run = dir.run(&["--json", "bogus"]);
    run.expect(2);
    let error = run.json();
    assert_eq!(error["exit_code"], 2);
    assert_eq!(error["kind"], "usage");
    assert!(error["error"].as_str().unwrap().contains("bogus"));

    // Empty or invalid input is a usage error too (checked before Bonsai is needed).
    let write = |extra: &[&str]| {
        let mut args = vec!["--url", DEAD_URL, "write", "--no-auto-start"];
        args.extend_from_slice(extra);
        dir.run(&args)
    };
    write(&["--mode", "r2v", "--request", "   "]).expect(2);
    write(&["--mode", "f2v", "--request", "Picture 1: a man.", "--refs", "3"]).expect(2);
    let spec = dir.path().join("spec.json");
    std::fs::write(&spec, "{ not json").unwrap();
    write(&["--spec-file", spec.to_str().unwrap()]).expect(2);
    write(&["--mode", "r2v", "--request-file", "does-not-exist.txt"]).expect(1);
    write(&["--mode", "r2v", "--stdin"]).expect(2);
}

#[test]
fn commands_that_need_bonsai_exit_3_when_it_is_not_running() {
    let dir = ConfigDir::new("unavailable");
    let r2v = std::fs::read_to_string(fixtures().join("good_r2v_e1.txt")).unwrap();
    write_entry(&dir, "20261028-101500-000", "r2v", "Ref 1: a woman. 3 s.", &r2v, json!({}));

    let run = dir.run(&[
        "--url",
        DEAD_URL,
        "write",
        "--mode",
        "r2v",
        "--request",
        "Ref 1: a woman. 6 s, one take: she waves.",
        "--no-auto-start",
        "--json",
    ]);
    run.expect(3);
    let error = run.json();
    assert_eq!(error["kind"], "unavailable");
    assert!(error["error"].as_str().unwrap().contains("127.0.0.1:9"), "{error}");

    // auto_start is off in the config: no start without --no-auto-start either.
    let run = dir.run(&["--url", DEAD_URL, "write", "--mode", "f2v", "--request", "Picture 1: a man. 3 s."]);
    run.expect(3);
    assert!(run.stderr.contains("auto_start is off"), "{}", run.stderr);

    // stdin request, follow-up fix, chat, tokens.
    dir.run_with_stdin(&["--url", DEAD_URL, "write", "--mode", "r2v", "--stdin", "--no-auto-start"], "Ref 1: a woman.")
        .expect(3);
    dir.run(&["--url", DEAD_URL, "fix", "--last", "--fix", "Make it longer.", "--no-auto-start"]).expect(3);
    dir.run(&["--url", DEAD_URL, "chat", "--message", "hi", "--no-auto-start"]).expect(3);
    let run = dir.run(&["--url", DEAD_URL, "tokens", "--text", "hi", "--json"]);
    run.expect(3);
    assert_eq!(run.json()["exit_code"], 3);

    let status = dir.run(&["--url", DEAD_URL, "status", "--json"]);
    status.expect(3);
    let status = status.json();
    assert_eq!(status["ready"], false);
    assert_ne!(status["state"], "ready");
    assert_eq!(status["url"], DEAD_URL);
    assert!(status["message"].is_string());
    dir.run(&["--url", DEAD_URL, "status"]).expect(3);
}

#[test]
fn fix_from_lint_needs_something_to_fix() {
    let dir = ConfigDir::new("from-lint");
    let good = std::fs::read_to_string(fixtures().join("good_f2v_e8.txt")).unwrap();
    let request = "Picture 1: a man at an oak table. Picture 2: the man at the door. 3 s, one shot: he walks out.";
    write_entry(&dir, "20261028-090000-000", "f2v", request, &good, json!({}));
    let run = dir.run(&["--url", DEAD_URL, "fix", "--last", "--from-lint", "--no-auto-start", "--json"]);
    run.expect(1);
    assert!(run.json()["error"].as_str().unwrap().contains("nothing to fix"), "{}", run.stdout);

    // A prompt with errors: the fix is built from the check, then Bonsai is needed.
    let bad = std::fs::read_to_string(fixtures().join("bad_r2v_markdown_fake_shots.txt")).unwrap();
    write_entry(
        &dir,
        "20261028-100000-000",
        "r2v",
        "Ref 1: a woman. Ref 2: a bedroom. 15 s, one take.",
        &bad,
        json!({}),
    );
    let run = dir.run(&["--url", DEAD_URL, "fix", "--id", "20261028-100000-000", "--from-lint", "--no-auto-start"]);
    run.expect(3);

    // --dry-run shows the fix that would be sent.
    let dry = dir.run(&["--url", DEAD_URL, "fix", "--id", "20261028-100000-000", "--from-lint", "--dry-run", "--json"]);
    dry.expect(0);
    let dry = dry.json();
    assert_eq!(dry["parent_id"], "20261028-100000-000");
    assert_eq!(dry["messages"], 4, "system, request, previous answer, fix");
    let fix = dry["request"].as_str().unwrap();
    assert!(fix.starts_with("Fix these problems and keep everything else the same:\n- "), "{fix}");
    assert!(fix.contains("Around 00:05.000"), "{fix}");
    assert_eq!(dry["lint_context"]["duration_s"], 15.0, "read from the saved request");
}

#[test]
fn fix_uses_the_check_facts_saved_with_the_entry() {
    let dir = ConfigDir::new("saved-facts");
    let good = std::fs::read_to_string(fixtures().join("good_f2v_e7.txt")).unwrap();
    // An r2v request that a fix turned into an f2v prompt: the saved facts know no pictures, and
    // the r2v text must not be read as f2v facts again (Ref 2 would mean a last frame).
    let saved = json!({ "mode": "f2v", "duration_s": 6.0, "ref_count": null, "first_frame_pinned": null, "has_last_frame": null });
    let request = "Ref 1: a woman. Ref 2: a cafe. 6 s, one take: she sits down.";
    write_entry(&dir, "20261028-110000-000", "f2v", request, &good, json!({ "lint_context": saved }));
    let dry = dir.run(&["--url", DEAD_URL, "fix", "--last", "--fix", "Make it longer.", "--dry-run", "--json"]);
    dry.expect(0);
    assert_eq!(dry.json()["lint_context"], saved);
}

#[test]
fn fixes_of_one_mode() {
    let dir = ConfigDir::new("fixes-mode");
    let all = dir.run(&["fixes", "--json"]).json();
    for mode in ["r2v", "f2v"] {
        let some = dir.run(&["fixes", "--mode", mode, "--json"]);
        some.expect(0);
        let some = some.json();
        let some = some.as_array().unwrap();
        assert!(!some.is_empty(), "{mode}");
        assert!(some.iter().all(|f| all.as_array().unwrap().contains(f)), "{mode}: {some:?}");
    }
    dir.run(&["fixes", "--mode", "t2v"]).expect(2);
}

#[test]
fn write_dry_run_shows_the_request_and_the_check_facts() {
    let dir = ConfigDir::new("dry-run");
    let request = "Ref 1: a young woman in a red coat. Ref 2: a rainy street at night (place). 6 s, one take: she \
                   turns and smiles.";
    let dry = dir.run(&["--url", DEAD_URL, "write", "--mode", "r2v", "--request", request, "--dry-run", "--json"]);
    dry.expect(0);
    let dry = dry.json();
    assert_eq!(dry["request"], request);
    assert_eq!(dry["messages"], 2);
    assert_eq!(dry["save_history"], true);
    assert_eq!(dry["params"]["thinking"], "off");
    assert_eq!(dry["params"]["temperature"], 0.4);
    assert_eq!(
        dry["lint_context"],
        json!({ "mode": "r2v", "duration_s": 6.0, "ref_count": 2, "first_frame_pinned": false, "has_last_frame": null })
    );
    // Options override what the request says.
    let args = ["--url", DEAD_URL, "write", "--mode", "r2v", "--request", request, "--duration", "8", "--pinned"];
    let dry = dir.run(&[&args[..], &["--refs", "3", "--no-history", "--dry-run", "--json"]].concat()).json();
    assert_eq!(dry["lint_context"]["duration_s"], 8.0);
    assert_eq!(dry["lint_context"]["ref_count"], 3);
    assert_eq!(dry["lint_context"]["first_frame_pinned"], true);
    assert_eq!(dry["save_history"], false);

    // A hand-written spec only needs the fields it uses; relative images are next to the spec.
    let spec = dir.path().join("form.json");
    let form = json!({
        "mode": "f2v",
        "refs": [{ "description": "a man at an oak table" }, { "role": "last_frame", "description": "the man at the door" }],
        "duration_s": 4,
        "action": "he stands up and walks to the door",
        "dialogue": [{ "words": "See you." }]
    });
    std::fs::write(&spec, form.to_string()).unwrap();
    let dry = dir.run(&["write", "--spec-file", spec.to_str().unwrap(), "--dry-run", "--json"]);
    dry.expect(0);
    let dry = dry.json();
    assert_eq!(dry["mode"], "f2v");
    let text = dry["request"].as_str().unwrap();
    assert!(text.starts_with("Picture 1: a man at an oak table (first frame)."), "{text}");
    assert!(text.contains("Picture 2: the man at the door (last frame)."), "{text}");
    assert_eq!(dry["lint_context"]["has_last_frame"], true);
    assert_eq!(dry["lint_context"]["ref_count"], 2);
    // --mode overrides the spec's mode.
    let dry = dir.run(&["write", "--spec-file", spec.to_str().unwrap(), "--mode", "r2v", "--dry-run", "--json"]);
    assert_eq!(dry.json()["mode"], "r2v");
    // A picture to send that does not exist is an error before Bonsai is needed.
    let missing = json!({ "refs": [{ "description": "x", "image_path": "nope.png", "send_image": true }] });
    std::fs::write(&spec, missing.to_string()).unwrap();
    let run = dir.run(&["write", "--spec-file", spec.to_str().unwrap(), "--dry-run"]);
    run.expect(1);
    assert!(run.stderr.contains("nope.png"), "{}", run.stderr);
}

#[test]
fn describe_uses_the_cache_and_checks_its_arguments() {
    let dir = ConfigDir::new("describe");
    let picture = dir.path().join("cafe.png");
    std::fs::write(&picture, b"cafe picture bytes").unwrap();
    let pic = picture.to_str().unwrap();
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, "x").unwrap();
    let missing = dir.path().join("missing.png");
    for args in [
        vec!["describe"],
        vec!["describe", pic, "--role", "hat"],
        vec!["--url", DEAD_URL, "describe", missing.to_str().unwrap()],
        vec!["--url", DEAD_URL, "describe", notes.to_str().unwrap()],
        vec!["--url", DEAD_URL, "write", "--mode", "r2v", "--request", "x", "--describe"],
    ] {
        let run = dir.run(&args);
        assert_eq!(
            run.code, 2,
            "{args:?}
stdout:
{}
stderr:
{}",
            run.stdout, run.stderr
        );
    }
    // Not described yet: Bonsai is needed (and not running).
    let run = dir.run(&["--url", DEAD_URL, "describe", pic, "--role", "place", "--json"]);
    run.expect(3);
    assert_eq!(run.json()["kind"], "unavailable");

    // Described before (same bytes and role): from the cache, no Bonsai.
    let key = bh3::describe::DescriptionCache::key(b"cafe picture bytes", bh3::request::RefRole::Place);
    let cache = dir.path().join("descriptions");
    std::fs::create_dir_all(&cache).unwrap();
    let saved = json!({ "description": "an empty sunlit cafe with tall windows", "model": "m.gguf", "created": "2026-10-28 12:00:00" });
    std::fs::write(cache.join(format!("{key}.json")), saved.to_string()).unwrap();
    let run = dir.run(&["--url", DEAD_URL, "describe", pic, "--role", "background", "--json"]);
    run.expect(0);
    let json = run.json();
    assert_eq!(json["description"], "an empty sunlit cafe with tall windows");
    assert_eq!(
        (&json["cached"], &json["role"], &json["started_server"]),
        (&json!(true), &json!("place"), &json!(false))
    );
    let run = dir.run(&["--url", DEAD_URL, "describe", pic, "--role", "place"]);
    run.expect(0);
    assert_eq!(run.stdout.trim(), "an empty sunlit cafe with tall windows");
    // Another role, or --no-cache: Bonsai is asked.
    dir.run(&["--url", DEAD_URL, "describe", pic, "--role", "person"]).expect(3);
    dir.run(&["--url", DEAD_URL, "describe", pic, "--role", "place", "--no-cache"]).expect(3);

    // write --describe fills the form's empty descriptions (here from the cache, so --dry-run shows it).
    let spec = dir.path().join("form.json");
    let form = json!({
        "mode": "r2v",
        "refs": [{ "role": "place", "image_path": "cafe.png" }, { "description": "a young woman", "image_path": "cafe.png" }],
        "duration_s": 6,
        "action": "she sits down"
    });
    std::fs::write(&spec, form.to_string()).unwrap();
    let dry = dir.run(&[
        "--url",
        DEAD_URL,
        "write",
        "--spec-file",
        spec.to_str().unwrap(),
        "--describe",
        "--dry-run",
        "--json",
    ]);
    dry.expect(0);
    let request = dry.json()["request"].as_str().unwrap().to_string();
    assert!(request.starts_with("Ref 1: an empty sunlit cafe with tall windows (place)."), "{request}");
    assert!(request.contains("Ref 2: a young woman (person)."), "typed text is kept: {request}");
}

#[test]
fn starting_without_a_bonsai_folder_exits_3() {
    let dir = ConfigDir::new("no-folder");
    let empty = dir.path().join("not-bonsai");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(dir.path().join("config.json"), json!({ "auto_start": true }).to_string()).unwrap();
    // Auto-start is allowed, but the folder is not a Bonsai-demo folder: nothing is started.
    let run = dir.run(&[
        "--url",
        DEAD_URL,
        "--bonsai-dir",
        empty.to_str().unwrap(),
        "write",
        "--mode",
        "r2v",
        "--request",
        "Ref 1: a woman. 3 s.",
        "--json",
    ]);
    run.expect(3);
    assert!(run.json()["error"].as_str().unwrap().contains("not a Bonsai-demo folder"), "{}", run.stdout);
}

/// The new-PC flow without a Bonsai-demo folder: `models use` keeps the files where they are and
/// finds the projector next to the model; `status` and `runtime show` name the missing server;
/// `--server` completes it; `remove` never deletes a file outside the app's models folder;
/// `models use none` goes back to bonsai_dir. Offline: nothing is downloaded or started.
#[test]
fn models_use_in_place_and_status_names_what_is_missing() {
    let dir = ConfigDir::new("models");
    let files = dir.path().join("elsewhere");
    std::fs::create_dir_all(&files).unwrap();
    let model = files.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf");
    let mmproj = files.join("Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf");
    std::fs::write(&model, vec![1u8; 4096]).unwrap();
    std::fs::write(&mmproj, vec![2u8; 512]).unwrap();
    let model_arg = model.to_str().unwrap();

    let used = dir.run(&["models", "use", model_arg, "--json"]);
    let used = used.expect(0).json();
    assert_eq!(used["model"], json!(model));
    assert_eq!(used["mmproj"], json!(mmproj), "found next to the model");
    assert_eq!(used["install"]["source"], "runtime");
    assert_eq!(used["install"]["usable"], false);
    let problems = used["install"]["problems"].to_string();
    assert!(problems.contains("No llama-server build"), "{problems}");
    assert!(model.is_file() && mmproj.is_file(), "nothing moved");

    let list = dir.run(&["models", "list", "--json"]).expect(0).json();
    let installed = list["installed"].as_array().unwrap();
    assert_eq!(installed.len(), 1, "{list}");
    assert_eq!((installed[0]["active"].clone(), installed[0]["managed"].clone()), (json!(true), json!(false)));
    assert_eq!(list["catalog"].as_array().unwrap().len(), 5);

    let status = dir.run(&["--url", DEAD_URL, "status", "--json"]);
    let status = status.expect(3).json();
    assert!(status["install"]["problems"].to_string().contains("llama-server"), "{status}");
    let shown = dir.run(&["runtime", "show", "--json"]).expect(0).json();
    assert_eq!(shown["active"], true);
    assert!(shown["recommended"]["backend"].is_string(), "{shown}");

    // A llama-server.exe anywhere on the PC completes it (not started here).
    let exe = files.join("llama-server.exe");
    std::fs::write(&exe, b"not a real program").unwrap();
    let full = dir.run(&["models", "use", model_arg, "--server", exe.to_str().unwrap(), "--json"]).expect(0).json();
    assert_eq!(full["install"]["usable"], true, "{full}");
    assert_eq!(full["server_exe"], json!(exe));

    // Not in the app's models folder: never deleted.
    let removed = dir.run(&["models", "remove", "Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0"]);
    removed.expect(2);
    assert!(model.is_file());

    dir.run(&["models", "use", "none", "--json"]).expect(0);
    assert_eq!(dir.run(&["config", "get", "runtime.model"]).expect(0).stdout.trim(), "none");
    assert_eq!(dir.run(&["models", "download", "no-such-model"]).code, 2);
    assert_eq!(dir.run(&["runtime", "download", "--backend", "rocm"]).code, 2);
}

// ---------------------------------------------------------------------------------------------
// Live tests (need Bonsai running on http://127.0.0.1:8080; never start or stop it)
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "needs the real Bonsai server on http://127.0.0.1:8080"]
fn live_status_is_ready() {
    let dir = ConfigDir::new("live-status");
    let status = dir.run(&["status", "--json"]);
    status.expect(0);
    let status = status.json();
    assert_eq!(status["ready"], true);
    assert!(status["props"]["n_ctx"].as_u64().unwrap() > 0);
    let tokens = dir.run(&["tokens", "--text", "subject_definitions:", "--json"]).json();
    assert!(tokens["tokens"].as_u64().unwrap() > 0);
}

#[test]
#[ignore = "needs the real Bonsai server on http://127.0.0.1:8080 (~45 s)"]
fn live_write_r2v_then_fix_it() {
    let dir = ConfigDir::new("live-write");
    let request = "Ref 1: a young woman with short black hair in a denim jacket. Ref 2: a rainy neon street at night \
                   (place). Use the place from Ref 2. 6 s, one take: she turns to the camera and smiles. Style: film look: \
                   35mm, soft grain, warm grade.";
    let run = dir.run(&["write", "--mode", "r2v", "--request", request, "--json", "--no-auto-start"]);
    run.expect(0);
    let written = run.json();
    assert_eq!(written["mode"], "r2v");
    assert!(written["prompt"].as_str().unwrap().starts_with("subject_definitions:"), "{written}");
    assert!(written["lint"]["score"].is_u64());
    assert_eq!(written["lint_context"]["duration_s"], 6.0);
    assert_eq!(written["lint_context"]["ref_count"], 2);
    assert_eq!(written["started_server"], false);
    let id = written["history_id"].as_str().expect("saved to the history").to_string();

    let run = dir.run(&[
        "fix",
        "--last",
        "--fix",
        "Remove the music (non_diegetic_music: N/A).",
        "--json",
        "--no-auto-start",
    ]);
    run.expect(0);
    let fixed = run.json();
    assert_eq!(fixed["parent_id"], id.as_str());
    assert!(fixed["prompt"].as_str().unwrap().contains("non_diegetic_music: N/A"), "{fixed}");
    let list = dir.run(&["history", "list", "--json"]).json();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["fix"], "Remove the music (non_diegetic_music: N/A).");
}

#[test]
#[ignore = "needs the real Bonsai server on http://127.0.0.1:8080 (~20 s)"]
fn live_write_f2v_without_history() {
    let dir = ConfigDir::new("live-f2v");
    let run = dir.run(&[
        "write",
        "--mode",
        "f2v",
        "--request",
        "Picture 1: a man in a navy raincoat at a rainy bus stop at night, medium shot. 4 s: the bus arrives and he \
         steps on board.",
        "--no-history",
        "--json",
        "--no-auto-start",
    ]);
    run.expect(0);
    let written = run.json();
    assert_eq!(written["mode"], "f2v");
    assert_eq!(written["history_id"], Value::Null);
    assert!(written["prompt"].as_str().unwrap().contains("integrated_multimodal_description: [Shot 1]"), "{written}");
    // The checker knows what the request said (one picture, no last frame), whatever Bonsai wrote.
    assert_eq!(written["lint_context"]["ref_count"], 1);
    assert_eq!(written["lint_context"]["has_last_frame"], false);
    assert_eq!(written["lint_context"]["duration_s"], 4.0);
    assert_eq!(dir.run(&["history", "list", "--json"]).json(), json!([]));
    let prompt_file = dir.path().join("p.txt");
    std::fs::write(&prompt_file, written["prompt"].as_str().unwrap()).unwrap();
    let lint = dir.run(&["lint", "--file", prompt_file.to_str().unwrap(), "--json"]).json();
    assert_eq!(lint["mode"], "f2v");
}
