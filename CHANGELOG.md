# Changelog

All notable changes to Bonsai H3 Writer are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The section of a version becomes the notes of its GitHub release (see `docs/RELEASING.md`).

## [Unreleased]

## [0.1.0] - 2026-10-28

First release: a desktop app, a command line tool and an MCP server that write MiniMax H3 video
prompts with a local Bonsai 27B and manage the Bonsai server for you.

### Added

- **Desktop app** (`bonsai-h3-writer.exe`) with a Blender-style layout: a top bar with menus,
  workspace tabs and a Bonsai power switch; a left toolbar, editor areas and a right N-panel in
  every workspace; a status bar; a dense dark theme with UI scale and accent colour settings.
- **Keyboard shortcuts**: Ctrl+1 ... Ctrl+6 pick a workspace, Ctrl+PageDown / Ctrl+PageUp go to the
  next / previous one, Ctrl+Plus / Ctrl+Minus / Ctrl+0 zoom the interface (saved as the UI scale),
  Ctrl+Enter writes or sends, Esc cancels, Ctrl+S saves a library item.
- **Write workspace**: r2v prompts (up to 9 reference pictures) and f2v prompts (first frame, or
  first + last frame). Request builder with reference roles, descriptions and optional images for
  Bonsai's vision input; shot duration with the real H3 clip length, cuts and action; style,
  camera, place and outfit; people without a photo; dialogue; notes. Or write the request as text,
  starting from a template. Streaming output, optional reasoning view, live prompt check with score
  and issues, one-click follow-up fixes, the pre-render checklist and last-run statistics (new and
  cached tokens, tokens per second, time). The draft is saved and comes back after a restart.
- **Chat workspace**: free chat with Bonsai that replaces its web UI - saved conversations, the H3
  system prompts or a custom one, image attachments, collapsible reasoning, a context usage bar,
  regenerate, stop and clear.
- **Library workspace**: edit the built-in system prompts, request templates, follow-up fixes,
  checklist and guides, with token counts. Edits are saved as overrides and can be reset to the
  built-in text at any time.
- **History workspace**: every written prompt is saved with its request, settings, timings, check
  score and the facts it was checked against (mode, clip length, pictures), so a later fix is
  checked the same way; search, mode filter, stars, open again in Write, export to `.txt`.
- **Bonsai workspace**: install detection, launch options (context size, vision projector on CPU,
  extra environment variables), a status card, a GPU card (VRAM, temperature, load, processes, a
  ComfyUI warning) and the live server log.
- **Settings**: UI scale, accent colour, monospace prompts, reasoning display, history, default
  clip duration, server URL and the default generation settings for prompts and chat.
- **Safe closing**: closing the window asks first while a prompt, fix or chat answer is still being
  written or the Library has unsaved edits.
- **Bonsai server management**: starts Bonsai through the Bonsai-demo launcher when a prompt or chat
  message needs it, waits for the model to load, stops it after idle minutes (default 10) and when
  the app closes if the app started it, refuses to auto-start while other programs hold the VRAM,
  and frees the GPU for ComfyUI with one click. Automatic stops end only the launcher the app
  started, and the idle stop waits while another client is still getting an answer; Stop ends only
  the server on the app's port (8080), never another program's `llama-server`.
- **Generation controls**: thinking Off / Low / Medium / High (Off by default - tested fastest and
  most reliable for this job), temperature (0.4 for prompts), max tokens, top-p, top-k, min-p and
  seed.
- **Prompt checker** for r2v and f2v prompts based on the official MiniMax H3 guides: sections and
  their order, task tags, picture numbers, retention markers, shot timestamps against the real clip
  length, the f2v first line, dialogue format and description length.
- **`bh3` command line tool** for scripts and LLM agents: `status`, `write`, `fix`, `lint`, `chat`,
  `history` and more, with `--json` output and documented exit codes (see `docs/CLI.md` and
  `docs/AGENT_GUIDE.md`).
- **MCP server**: `bh3 mcp` gives Claude Code and other MCP clients tools to write, fix and check H3
  prompts with the local Bonsai (`claude mcp add bonsai-h3 -- "<path>\bh3.exe" mcp`, see
  `docs/MCP.md`).
- **Built-in library** tested on Bonsai 27B with thinking off: r2v and f2v system prompts, request
  templates, follow-up fixes for each mode, a checklist and short readable guides.
- **Release zip and GitHub workflows**: CI (format, clippy, tests and a release build on Windows)
  and a tag-triggered release that publishes a zip with both executables, the README, this
  changelog, the license and the CLI, agent and MCP guides, plus a SHA-256 checksum. Both
  workflows use the Rust version pinned in `rust-toolchain.toml` (1.99.0); `scripts/package.ps1`
  builds the same zip locally.
