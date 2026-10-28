# bh3 mcp - MCP server for AI agents

`bh3 mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io) server on
stdin/stdout. Claude Code and other MCP clients then use Bonsai H3 Writer as native tools: write
a MiniMax H3 prompt with the local Bonsai 27B, check it, fix it, and start or stop Bonsai. The
tools do what the `bh3` commands do ([CLI.md](CLI.md)) and share the app's settings, Bonsai
folder, library texts and history.

## Setup

### Claude Code

```powershell
claude mcp add bonsai-h3 -- "C:\path\to\bh3.exe" mcp
```

Add `--scope user` after `add` to use it in every project. `claude mcp list` (or `/mcp` inside
Claude Code) shows whether it is connected. Global options go before `mcp`, e.g.
`-- "C:\path\to\bh3.exe" --bonsai-dir D:\Bonsai-demo mcp`.

### Other clients

The usual JSON configuration (Claude Desktop, Cursor, VS Code and others use this form):

```json
{
  "mcpServers": {
    "bonsai-h3": {
      "command": "C:\\path\\to\\bh3.exe",
      "args": ["mcp"]
    }
  }
}
```

The client starts `bh3 mcp` itself; it runs until the client closes its stdin (exit 0).
`BH3_CONFIG_DIR` in the server's environment selects another settings folder, as for every
`bh3` command.

## Tools

| Tool | Arguments | What it does |
|---|---|---|
| `bonsai_status` | - | state (`ready`, `loading`, `starting`, `stopped`, `failed`), URL, model, context, slots, vision, the Bonsai folder, `auto_start`, GPU memory and temperature, other programs on the GPU. Changes nothing |
| `write_prompt` | `mode` (`r2v` / `f2v`), `request`; optional `duration_s`, `refs`, `first_frame_pinned`, `has_last_frame`, `images` (absolute paths), `thinking`, `temperature`, `max_tokens` (256-16384), `auto_start`, `save_history` | writes, cleans, checks and saves a prompt, like `bh3 write` |
| `fix_prompt` | `fix` or `from_lint: true`; optional `history_id` (default `last`), `thinking`, `temperature`, `max_tokens`, `auto_start`, `save_history` | follow-up fix of a saved prompt, saved as a new entry, like `bh3 fix` |
| `lint_prompt` | `prompt`; optional `mode`, `duration_s`, `refs`, `first_frame_pinned`, `has_last_frame` | checks any prompt without Bonsai, like `bh3 lint` |
| `clip_length` | `seconds` | frames, real length (`6.58` for 6 s), suggested most cuts, laptop size limit |
| `get_guide` | `mode` | the short H3 guide of the mode and the official guide's URL |
| `list_templates` | optional `mode` | request templates with `<...>` parts to fill |
| `list_history` | optional `limit` (default 20, max 200), `search`, `starred` | saved prompts, newest first |
| `get_history_entry` | `id` (or `last`) | one saved entry with a fresh check report (`lint`) |
| `start_bonsai` | optional `wait` (default true), `force`, `timeout_s` | like `bh3 start [--wait] [--force]` |
| `stop_bonsai` | - | like `bh3 stop`: ends the `llama-server.exe` on Bonsai's port and frees the GPU |
| `chat` | `message`; optional `system` (`none`, `r2v`, `f2v` or your own text), `thinking`, `temperature`, `max_tokens`, `auto_start` | one question to Bonsai, nothing saved |

`thinking` is `off` (the tested default, valid prompts in 10-25 s), `low`, `medium` or `high`.
The check facts of `write_prompt` are read from the request like `bh3 write` does; pass
`duration_s`, `refs`, `first_frame_pinned` and `has_last_frame` when the text is ambiguous (ages
such as "in her 20s"). A fix keeps the facts its entry was checked with.

## Results

- `write_prompt` / `fix_prompt` text: the prompt, a blank line, `Check: score 92 - 0 errors, 1
  warning, 0 info (r2v, 6 s, 2 pictures)` with one line per error and warning, `History id: ...`
  and warnings. `structuredContent`: `prompt`, `mode`, `lint` (`ok`, `score`, `errors`,
  `warnings`, `infos`, `issues[]` with `severity`, `code`, `message`, `line`), `lint_context`,
  `history_id`, `parent_id`, `started_server`, `finish_reason`, `timings`, `warnings`.
- The prompt goes into MiniMax H3 unchanged; problems are fixed with `fix_prompt`, not by hand.
- A failed call is a normal result with `isError: true`: the text says why (with the tool to
  use, e.g. "Start it with start_bonsai."), `structuredContent` is `{"error", "kind"}` with
  `kind` `unavailable` (Bonsai not running or could not start), `generation` (empty or failed
  answer), `usage` (a bad argument), `failure` (e.g. unknown history id) or `cancelled`.
- Unknown tools, arguments that are not an object, unknown methods and broken JSON are JSON-RPC
  errors (-32602, -32601, -32700).

## Typical flow

1. `write_prompt` with `mode` and a request in plain words (`list_templates` shows the form).
2. `structuredContent.lint.ok` false: `fix_prompt` with `from_lint: true`, or with `fix` in your
   own words. One round is usually enough; a badly wrong prompt is written again instead.
3. Give the user `structuredContent.prompt`.
4. The user renders in ComfyUI next: `stop_bonsai` (only when they want the GPU back).

## Bonsai and the GPU

- `write_prompt`, `fix_prompt` and `chat` start Bonsai when it is not running and auto-start is
  on (the app's `auto_start`; the `auto_start` argument overrides it for one call). Loading takes
  about a minute; the call waits up to `start_timeout_secs` (300 s).
- A start is refused while other programs (ComfyUI's `python.exe`?) hold more than
  `vram_busy_mb` of GPU memory: Bonsai needs about 7.7 GB of the 8 GB. `start_bonsai` with
  `force: true` starts anyway; do that only when the user agrees.
- No tool except `stop_bonsai` stops Bonsai, and it keeps running when the client exits. The
  app's idle auto-stop only covers a Bonsai the app started.
- `notifications/cancelled` stops the running call: an answer being written is aborted on the
  server; a start keeps loading in the background.

## Troubleshooting

- Notes and progress go to stderr; clients keep them in their MCP logs. The server log of
  llama-server: `bh3 logs`.
- "Bonsai is not running ... (auto_start is off in the settings)": call `start_bonsai`, pass
  `auto_start: true`, or turn it on with `bh3 config set auto_start true`.
- "Not starting Bonsai: python.exe (ComfyUI?) uses ...": close ComfyUI, or `start_bonsai` with
  `force: true`.
- No Bonsai folder found: `bh3 install set D:\Bonsai-demo`, or add `--bonsai-dir` before `mcp`.
- Try it by hand in PowerShell: `'{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | bh3 mcp`.

Protocol: newline-delimited JSON-RPC 2.0 over stdio, UTF-8; protocol versions 2024-11-05,
2025-03-26 and 2025-06-18 (others get 2025-06-18); the `tools` capability only. A batch (JSON
array) is answered with an array; notifications get no answer.
