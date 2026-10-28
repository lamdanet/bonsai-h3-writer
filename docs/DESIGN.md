# Bonsai H3 Writer - design

Desktop app (Rust, eframe/egui 0.31) + CLI (`bh3`, also an MCP server) that drives the user's local
**Bonsai 27B** (llama.cpp `llama-server` from the Bonsai-demo repo) to write **MiniMax H3** video
prompts that follow the official guides. It replaces Bonsai's web UI for this job and manages the
server.

## Environment it is built for
- Windows 11, RTX 4060 Laptop **8 GB**, 16 GB RAM. Bonsai-demo at `D:\Bonsai-demo`.
- Bonsai is started by `scripts\start_llama_server.ps1` (127.0.0.1:**8080**, model
  `models\bonsai2-gguf\27B\*-PTQ1_0.gguf` + mmproj, CUDA build `bin\cuda\llama-server.exe`, ctx 16384).
  It needs ~7.7 GB VRAM, so **ComfyUI cannot run at the same time** -> the app auto-starts Bonsai
  when a prompt is requested and auto-stops it after idle minutes to free the GPU.
- Tested generation settings: **thinking off** (`chat_template_kwargs: {"enable_thinking": false}`),
  temperature 0.4 -> valid prompts in 10-25 s. Thinking `low`/`xhigh` burned 1-3k tokens without
  answering. The system prompt is cached by llama-server when it stays byte-identical.

## Crate layout
| Path | What |
|---|---|
| `src/lib.rs` (`bh3`) | core library, no UI |
| `src/gen.rs` | Mode, ThinkingMode, GenParams, ChatMessage, request body (done) |
| `src/lengths.rs` | H3 24 fps 17k+5 grid, real lengths, timestamps (done) |
| `src/config.rs` | AppConfig in `%APPDATA%\BonsaiH3Writer\config.json` (done) |
| `src/client.rs` | llama-server HTTP client (ureq 2), SSE streaming |
| `src/server.rs` | install detection, start/stop via the launcher or directly (app runtime), state, log |
| `src/runtime.rs` | model / runtime manager: catalog, server builds, resumable downloads, zip extraction, import, VC++ check |
| `src/gpu.rs` | nvidia-smi status |
| `src/lint.rs` | prompt checker (r2v/f2v rules) |
| `src/request.rs` | structured request form -> request text |
| `src/library.rs` | built-in texts (`assets/library/*`) + user overrides |
| `src/history.rs` | saved prompts (`history\<id>.json`) |
| `src/writer.rs` | messages, output cleanup, lint, history for one job |
| `src/describe.rs` | describe a reference picture once with the vision input: request per role, cleanup, cache by sha256 + role |
| `src/service.rs` | GUI background thread (commands in, events out) |
| `src/cli/mod.rs` | `bh3` CLI (clap): commands, `--json` reports, exit codes; `src/bin/bh3.rs` only calls it |
| `src/cli/models.rs` | `bh3 models ...` / `bh3 runtime ...` |
| `src/cli/mcp.rs` | `bh3 mcp`: MCP server (JSON-RPC over stdin/stdout) whose tools run the CLI's code paths (docs/MCP.md) |
| `src/main.rs`, `src/gui/*` | desktop app (bin `bonsai-h3-writer`) |
| `rust-toolchain.toml` | pinned Rust (1.99.0) for local builds, CI and releases; bump it on purpose |

## App runtime (no Bonsai-demo folder)
- `config.runtime` (`RuntimeConfig`): `model`, `mmproj`, `server_exe` paths (anywhere: files are
  used in place), `dir` / `models_dir` (storage, default `%LOCALAPPDATA%\BonsaiH3Writer\runtime`),
  `hf_token`. A set `model` takes precedence over `bonsai_dir` (`server::install_from_config`);
  without `server_exe` the newest build in `<runtime>\bin` is used, else `bonsai_dir`'s.
- `BonsaiInstall::from_runtime` gives an install with `runtime: true` (root = the server's folder,
  `models` = the one model); `ServerManager` always starts it directly (`direct_args`: the
  launcher's Bonsai 2 flags incl. sampling, `--webui-config-file` when imported, extra args last)
  and re-checks the VC++ DLLs (`missing_dlls`) before each start.
- Launch options `gpu_layers`, `image_max_tokens`, `kv_cache_4bit` are passed as the launcher's
  `BONSAI_NGL` / `BONSAI_IMAGE_MAX_TOKENS` / `BONSAI_KV4` (both start paths read them);
  `extra_args` go last (to the launcher's `@args` in folder mode).
- Tasks (`runtime::RuntimeTask`: model download, server download, import) run on their own
  service thread (`Command::RunTask` / `CancelTask`, `Event::TaskProgress/Done/Failed/Cancelled`),
  several at once but never two with the same targets; while one writes or moves the chosen
  install's files, Start is refused. The UI applies a finished task (`models_ws::Then`) and sends
  `SetInstall`. The CLI runs the same `runtime::run_task` in the foreground (Ctrl+C = cancel).
- Downloads: `<file>.part`, `Range` resume, redirects followed by hand (token only to its host),
  sha256 from the catalog / Hugging Face tree API / GitHub release digests, free-space check first.
- GUI: Setup dialog ways (Files / Download / Folder), Bonsai workspace bottom area tabs Log /
  Models (`gui/models_ws.rs`), Settings > Bonsai server > Model and Server Files + launch options.
  Screenshot helpers: `BH3_WORKSPACE=models`, `BH3_SETUP_WAY=files|download|folder`,
  `BH3_MODELS_SCROLL=<px>`.

## Contract rules (parallel work)
- Public items in the stub files are the contract. **Do not rename/remove/change signatures or
  struct fields.** You may add private helpers, new public functions/types, tests, and (GUI) fields
  to your own workspace state struct.
- Keep the crate compiling: write whole files, then fix errors fast. Only fix errors in files you own.
- No `unwrap()`/`expect()` on fallible runtime paths outside tests; use `anyhow` with context.
- Windows-only code behind `#[cfg(windows)]` with a portable fallback, so `cargo check` works elsewhere.
- Hidden child processes: `std::os::windows::process::CommandExt::creation_flags(0x0800_0000)` (CREATE_NO_WINDOW).
- Live server for tests: `http://127.0.0.1:8080` may be running. Only health/props/slots/tokenize and
  tiny chats (thinking off, `max_tokens <= 60`). **Never stop or start the real server** unless the
  task explicitly says so.

## Picture descriptions
- A reference picture is described **once** by Bonsai's vision input (`describe::messages`: a fixed
  system prompt, the role's instruction from `describe::instruction`, the picture as an
  `image_url` data URI; thinking off, temperature 0.2, max 220 tokens), cleaned to one paragraph
  (`clean_description`) and put into the ref's `description`; the prompt request then sends only
  text. `RefImage::auto_description` keeps Bonsai's text: while `description` equals it, the text
  is Bonsai's and may be replaced (re-describe, role change); once edited it is the user's and is
  never overwritten (`describe::apply`; a late description is offered instead).
- Cache: `config_dir()\descriptions\<sha256 of the file bytes>-<role id>.json`
  (`{description, model, created}`); a re-describe skips it. A server without a projector
  (`ServerProps::vision` false) fails with `describe::NO_VISION`.
- GUI: `JobKind::Describe { image, role, use_cache }` runs through the same queue and auto-start as
  other jobs (a cache hit answers at once in `submit`, without a server); `Event::DescribeDone`
  carries the `Described`, routed by job id to the cards with that picture and role. Auto describe
  (`AppConfig::auto_describe`, default on; never with `BH3_DEMO=1`) runs when a picture is picked
  for a card whose text is empty or Bonsai's; the Write button waits while a used ref is described.
  Describe jobs are not "unsaved work" for the close dialog.
- CLI `bh3 describe <image> --role R [--no-cache]`, `bh3 write --spec-file F --describe`; MCP
  `describe_image`.

## Threading model (GUI)
UI thread never blocks. `service::spawn` runs a control thread that owns `ServerManager`, polls status
(2 s), GPU (3 s), log (2 s), runs one job at a time on a worker thread with a cancel flag, and sends
`Event`s; it calls `repaint` after sending. Workspaces react in `on_event` (match their own job ids).

## Bonsai server lifecycle (who stops what)
- There is one Bonsai: the launcher always serves on 127.0.0.1:8080 (the configured server URL).
  `ServerManager` uses a server that already answers there, otherwise starts the launcher hidden and
  waits for `/health`; it remembers whether it started the server (`started_by_us`).
- Automatic stops - idle (`auto_stop_idle_minutes`), exit (`stop_on_exit`, on `Command::Shutdown`
  from `on_exit`) and `bh3 ... --stop-after` - only happen when `started_by_us`, and end only our
  launcher's own process tree (`taskkill /F /T /PID <launcher>`).
- Manual Stop / Restart / Free GPU also stop a Bonsai the user started, but only the `llama-server`
  that owns our port. Another program's `llama-server` (another port) is never stopped or killed.
- The idle stop respects other clients: our jobs reset the idle clock, and before stopping, the
  control thread asks llama-server (`/slots`) whether a request is still being processed - the web
  UI, `bh3` or a script using the same server - and then waits instead of stopping mid-answer.

## Saved state and closing (GUI)
- The Write workspace autosaves its draft in the config folder and restores it on the next start.
- Close confirmation: a close request (window X, File > Quit, Alt+F4) while a job is running or
  `LibraryState::has_unsaved()` is answered with `ViewportCommand::CancelClose` and a dialog that asks
  before the work is lost; only a confirmed close reaches `on_exit` (which sends `Shutdown`).

## UI: Blender conventions
- Colours/sizes from `gui/theme.rs` (`Palette`), building blocks from `gui/widgets.rs`
  (`area_header`, `panel`, `prop_row`, `enum_buttons`, `primary_button`, `icon_button`, `status_dot`,
  `badge`, `section_label`, `copy_button`, `code_view`, `thin_separator`, `fmt_mb`, `fmt_secs`, `icons`).
- Window = top bar (menus + workspace tabs + Bonsai power switch) / workspace / status bar.
- A workspace = **left toolbar region** (`SidePanel::left`, fill `Palette::SIDE`), **main editor area(s)**
  (`CentralPanel`, fill `Palette::REGION`, each area starts with an `area_header` strip), **right
  N-panel** (`SidePanel::right`, fill `Palette::SIDE`) holding collapsible `panel`s of `prop_row`s.
  Areas are separated by thin `Palette::GAP` lines (panel frames with 1-2 px gap fill).
- Dense, small text (13.5 pt), labels dim and right-aligned in property rows, blue for active widgets,
  orange accent only for the active workspace tab and the main action.
- Every action reachable by mouse; important ones also by shortcut (Ctrl+Enter write/send,
  Esc cancel, Ctrl+S save, Ctrl+C copy prompt when not editing). Global ones live in `gui/topbar.rs`:
  Ctrl+1..6 workspaces, Ctrl+PageDown/PageUp next/previous workspace, Ctrl+Plus/Minus/0 zoom (egui's
  own keyboard zoom is turned off; the app's zoom is saved as `ui_scale`, 0.75-2.0).
- Screenshot check: `BH3_SCREENSHOT=out.png BH3_WORKSPACE=write cargo run` saves a PNG after ~45
  frames and exits (`BH3_SETUP=1` shows the setup dialog, `BH3_SETUP=0` hides it).

### Workspaces
- **Write** - left: Mode (r2v / f2v), Templates (click fills the text request), Quick tips +
  "Describe pictures automatically". Centre top: **Request** area with tabs *Builder* (References
  list: role, description (auto-described from the picture, with state row: describing + Cancel /
  written by Bonsai + Re-describe / error / offer / Describe with Bonsai), optional
  image with thumbnail + "send to Bonsai" toggle (off by default) and token warning, reorder/remove, up to 9 (r2v) or
  first/last (f2v); Shot: duration with real length, cuts, action; Look: style, camera, place, outfit,
  people without photo; Dialogue rows; Notes) and *Text* (the built request, editable; "Rebuild from
  form" when edited). Main button **Write prompt** (Ctrl+Enter) / Cancel (Esc) + phase text.
  Centre bottom: **Prompt** area: reasoning (collapsible, when thinking on), the prompt (monospace,
  editable, re-linted on edit), Copy / Save .txt, word count, lint badge. Saved prompts keep the
  facts they were checked against (`HistoryEntry::lint_context`, read with
  `writer::entry_lint_context`), so a later fix is linted with the same mode, length and pictures.
  Right N-panel: Bonsai (status, start/stop, auto-start), Generation (thinking buttons + hint,
  temperature, max tokens, advanced: top-p/top-k/min-p/seed), Check (lint score + issues),
  Fix (`Library::fixes_for(mode)` as buttons + custom fix -> follow-up job), Checklist, Last run (tokens new/cached,
  generated, tok/s, time).
- **Chat** - left: conversations (new, rename, delete; saved in `chats\*.json`). Centre: messages
  (role, collapsible reasoning, copy), input (Enter send, Shift+Enter newline), attach images,
  Stop. Right: system prompt (None / H3 r2v / H3 f2v / custom), generation settings, context usage
  bar (prompt tokens / n_ctx), regenerate, clear.
- **Library** - left: items (edited badge). Centre: editor with Save (Ctrl+S), Revert, Reset to
  built-in, Copy, token count (via service when Ready). Right: what the item does, parsed preview
  (templates list, fixes count, checklist items).
- **History** - left: search, mode filter, starred only. Centre: table (star, date, mode, title,
  score, tokens/s). Bottom/right: request + prompt preview, Copy, Open in Write, Export .txt, Delete, Star.
- **Bonsai** (server) - left: Install (folder, change, detected parts with check marks), Launch
  options (auto-start, auto-stop minutes, stop on exit, VRAM busy threshold, context size, mmproj on
  CPU, extra env). Centre: status card (state, model, ftype, ctx, slots, vision, build, uptime, idle /
  auto-stop countdown) with Start / Stop / Restart / Open web UI / Free GPU for ComfyUI; GPU card
  (VRAM bar, temp, util, processes - warn about python.exe/ComfyUI); live log (filter, autoscroll, copy,
  open file).
- **Settings** - UI scale, accent colour, monospace prompts, show reasoning, save history, default
  duration, server URL, default writer/chat generation settings, config folder, reset, About.
- **Setup dialog** (first run / Bonsai > Change folder): three ways - model files on this PC (in place;
  server download when none is known), Download Bonsai (sizes, free space), Bonsai-demo folder
  (detected installs, Browse..., validation list, Use this folder, Import into the app); Skip.
