# bh3 - command line reference

`bh3.exe` is the command line tool of Bonsai H3 Writer. It writes MiniMax H3 video prompts with
your local Bonsai 27B, checks them against the official H3 format and manages the Bonsai server,
like the desktop app, for scripts and LLM agents (Claude Code and others).

- It uses the app's settings, Bonsai folder, library texts and history
  (`%APPDATA%\BonsaiH3Writer`, or the folder in `BH3_CONFIG_DIR`).
- It never asks questions and never waits for typing.
- Results go to **stdout**, progress and notes to **stderr**. With `--json`, stdout is exactly
  one JSON document, also when the command fails.
- Exit codes say what happened (see [Exit codes](#exit-codes)).
- `write`, `fix` and `chat` start Bonsai when it is not running and leave it running; `--stop-after`
  stops it again if that command started it. `bh3 stop` frees the GPU at any time.

A compact guide for LLM agents is in [AGENT_GUIDE.md](AGENT_GUIDE.md) (also `bh3 agent-guide`).

## Contents

- [Quick start](#quick-start)
- [Global options](#global-options)
- [Exit codes](#exit-codes)
- [Server: status, start, stop, restart](#server-status-start-stop-restart)
- [write](#write)
- [describe](#describe)
- [fix](#fix)
- [lint](#lint)
- [chat](#chat)
- [Library: templates, template, fixes, system-prompt, guide](#library-templates-template-fixes-system-prompt-guide)
- [history](#history)
- [lengths](#lengths)
- [tokens](#tokens)
- [config](#config)
- [install](#install)
- [models](#models)
- [runtime](#runtime)
- [gpu, logs, agent-guide, version](#gpu-logs-agent-guide-version)
- [mcp](#mcp)
- [Request spec files](#request-spec-files)
- [Check codes](#check-codes)
- [Text encoding on Windows](#text-encoding-on-windows)
- [Recipes](#recipes)

## Quick start

```powershell
bh3 status                                   # is Bonsai ready? (exit 0 = yes, 3 = no)
bh3 write --mode r2v --request "Ref 1: a young woman with short black hair in a denim jacket. Ref 2: a rainy neon street at night (place). 6 s, one take: she turns to the camera and smiles."
bh3 fix --last --from-lint                   # let Bonsai fix what the check found
bh3 history show last --field prompt         # the newest prompt again
bh3 stop                                     # free the GPU for ComfyUI
```

`bh3 --help` and `bh3 <command> --help` list every option.

## Global options

These work with every command, before or after its name.

| Option | Meaning |
|---|---|
| `--url <URL>` | Bonsai server address. Default: `server_url` from the config (`http://127.0.0.1:8080`). The Bonsai launcher always serves on port 8080, so another URL is only useful for a server you run yourself. |
| `--bonsai-dir <DIR>` | Bonsai-demo folder for starting Bonsai. Default: `bonsai_dir` from the config; when none is set, the first usable folder `bh3 install detect` finds. |
| `--json` | Print the result as one JSON document on stdout. A failure prints `{"error": "...", "exit_code": N, "kind": "..."}` instead (and the message on stderr), except for the exits listed under [Exit codes](#exit-codes) that print the normal result. |
| `-q`, `--quiet` | No progress messages on stderr. Warnings and errors are still printed. |

Help and version: `-h` (short help), `--help` (long help with all possible values), `-V` / `--version`.

## Exit codes

| Code | `kind` | Meaning |
|---|---|---|
| 0 | `ok` | success |
| 1 | `failure` | something failed (file not found, entry not found, nothing to fix, ...). `lint` and `write --strict`: the prompt has check errors |
| 2 | `usage` | wrong arguments or unusable input (unknown option, missing value, empty request, bad spec JSON, unknown setting) |
| 3 | `unavailable` | Bonsai is not running and could not be started (no Bonsai folder, auto-start off, GPU busy, start failed or timed out) |
| 4 | `generation` | Bonsai answered with an error or an empty answer |
| 130 | `cancelled` | cancelled with Ctrl+C (the request is aborted on the server; a start keeps going) |

The first Ctrl+C while Bonsai starts or writes cancels cleanly; a second Ctrl+C ends `bh3` at once.

With `--json` these non-zero exits print the normal result, not the error object: `status`
(exit 3, `message` says why), `lint` and `write` / `fix --strict` (exit 1, the report shows the
errors), `install show` (exit 1, see `problems`) and an `--out` file that cannot be written
(exit 1: the `write` / `fix` / `chat` result with the problem in `warnings`; the prompt was
written and saved to the history, so do not write it again).

JSON error example (`bh3 --url http://127.0.0.1:9 write ... --no-auto-start --json`, exit 3):

```json
{
  "error": "Bonsai is not running at http://127.0.0.1:9 (--no-auto-start). Start it with `bh3 start --wait`.",
  "exit_code": 3,
  "kind": "unavailable"
}
```

## Server: status, start, stop, restart

### status

```text
bh3 status [--json]
```

Server state, the model, the Bonsai folder and the GPU. Exit 0 when Bonsai is ready, 3 otherwise.

```text
Bonsai   Ready - http://127.0.0.1:8080
Model    Ternary-Bonsai-2-27B-PTQ1_0.gguf (PTQ1_0 - 1.75 bpw ternary (group 128))
Server   context 16384, 4 slots, vision on, build b10787-73bfe6cc4
Install  D:\Bonsai-demo (CUDA, usable, from the config)
GPU      NVIDIA GeForce RTX 4060 Laptop GPU: 7.6 GB of 8.0 GB used (95%), 49 °C, load 0%
         PID 22240   llama-server.exe (llama.cpp)
```

JSON:

```json
{
  "state": "ready",
  "state_label": "Ready",
  "ready": true,
  "url": "http://127.0.0.1:8080",
  "message": null,
  "llama_server_pids": [22240],
  "install": {
    "root": "D:\\Bonsai-demo",
    "source": "config",
    "usable": true,
    "problems": [],
    "warnings": [],
    "launcher": "D:\\Bonsai-demo\\scripts\\start_llama_server.ps1",
    "server_exe": "D:\\Bonsai-demo\\bin\\cuda\\llama-server.exe",
    "backend": "CUDA",
    "model": "D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B\\Ternary-Bonsai-2-27B-PTQ1_0.gguf",
    "models": ["D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B\\Ternary-Bonsai-2-27B-PTQ1_0.gguf"],
    "mmproj": "D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B\\Ternary-Bonsai-2-27B-mmproj-Q8_0.gguf"
  },
  "props": {
    "model_path": "D:\\Bonsai-demo\\models\\bonsai2-gguf\\27B\\Ternary-Bonsai-2-27B-PTQ1_0.gguf",
    "model_name": "Ternary-Bonsai-2-27B-PTQ1_0.gguf",
    "model_ftype": "PTQ1_0 - 1.75 bpw ternary (group 128)",
    "n_ctx": 16384,
    "total_slots": 4,
    "vision": true,
    "build_info": "b10787-73bfe6cc4"
  },
  "gpu": {
    "name": "NVIDIA GeForce RTX 4060 Laptop GPU",
    "mem_used_mb": 7775,
    "mem_total_mb": 8188,
    "temp_c": 49,
    "util_pct": 0,
    "processes": [{ "pid": 22240, "name": "llama-server.exe", "mem_mb": 0 }],
    "mem_free_mb": 413,
    "vram_used_by_others_mb": 0,
    "other_processes": [],
    "vram_busy_mb": 1500,
    "busy": null
  }
}
```

- `state`: `stopped`, `starting`, `loading` (the model loads), `ready`, `stopping` or `failed`.
- `message`: why Bonsai is not ready, e.g. "Nothing answers on ..." or "llama-server.exe is
  running (PID ...) but does not answer ...". `null` when ready.
- `install`: `null` when no model, no folder is set and none was found. `source` is `option`
  (`--bonsai-dir`), `config`, `detected` or `runtime` (the app's own runtime, `runtime.model` set:
  `root` is the server's folder, `launcher` is `null`). A folder that is not a Bonsai-demo folder has
  `usable: false` and the reason in `problems`; for the runtime `problems` names what is missing
  (no llama-server build, a model file not found, the Visual C++ runtime).
- `props`: `null` unless ready. `gpu`: `null` without `nvidia-smi`.
- `gpu.busy`: why a start without `--force` would be refused right now (other programs use more
  than `vram_busy_mb` of VRAM; on Windows the per-process memory is usually unknown, `mem_mb: 0`).

### start

```text
bh3 start [--wait] [--timeout SECS] [--force]
```

Starts Bonsai through `scripts\start_llama_server.ps1` of the Bonsai folder, hidden, with the
launch options from the config (`ctx_size` as `BONSAI_CTX`, `mmproj_cpu` as
`BONSAI_MMPROJ_CPU=1`). The server keeps running after `bh3` exits; its console output goes to
`server.log` (`bh3 logs`).

| Option | Meaning |
|---|---|
| `--wait` | Wait until the model is loaded (about a minute). Exit 3 when the start fails or times out. |
| `--timeout SECS` | How long `--wait` waits. Default: `start_timeout_secs` (300). |
| `--force` | Start even while other programs (ComfyUI's `python.exe`?) use more than `vram_busy_mb` of VRAM. Without it such a start is refused with exit 3 and the programs are named. |

Already running: exit 0 and "Bonsai is already running". Already loading: exit 0 (with `--wait`:
waits for it).

### stop

```text
bh3 stop
```

Stops Bonsai: the `llama-server.exe` that listens on the server URL's port (also one started by
`start_bonsai.bat` or by the app) and a launcher this command started, then waits until the GPU memory
is free. Other llama-server processes (another port, another app) keep running. Exit 0 also when nothing was running.

### restart

```text
bh3 restart [--wait] [--timeout SECS] [--force]
```

`stop` (when something runs), then `start` with the same options.

JSON of `start`, `stop` and `restart`:

```json
{
  "action": "start",
  "state": "ready",
  "state_label": "Ready",
  "ready": true,
  "url": "http://127.0.0.1:8080",
  "started": true,
  "stopped": false,
  "pid": 12345,
  "log": "C:\\Users\\you\\AppData\\Roaming\\BonsaiH3Writer\\server.log",
  "install": "D:\\Bonsai-demo",
  "message": "Bonsai is ready at http://127.0.0.1:8080 (48 s)."
}
```

`started` / `stopped` say what this command did; `pid`, `log` and `install` are `null` when
nothing was started.

## write

```text
bh3 write --mode r2v|f2v (--request TEXT | --request-file FILE | --spec-file FILE | --stdin) [options]
```

Sends the request to Bonsai with the system prompt of the mode, cleans the answer (code fences,
lead-ins, markdown headings, trailing notes), checks it and saves it to the history.

**Input** (exactly one):

| Option | Meaning |
|---|---|
| `--request TEXT` | the request in plain words |
| `--request-file FILE` | the request from a text file (UTF-8, or UTF-16 with BOM) |
| `--spec-file FILE` | a request form as JSON, see [Request spec files](#request-spec-files); it also gives the mode and the check facts |
| `--stdin` | the request from stdin |
| `--mode r2v\|f2v` | the prompt kind; required unless `--spec-file` gives it (then it overrides the spec's mode) |
| `--image PATH` | a picture for Bonsai's vision input, repeatable, in Ref order. Costs ~1,100 tokens and ~18 s per picture with every prompt on the laptop; describing it once with [`bh3 describe`](#describe) and putting the text into the request is cheaper |
| `--describe` | with `--spec-file`: Bonsai first describes the form's pictures that have no description (see [describe](#describe)) |

Write the request like the templates (`bh3 templates`): one line per picture (`Ref 1: ...` for
r2v, `Picture 1: ...` for f2v), the length and cuts (`6 s, one take: ...`), the action, camera and
style, dialogue in quotes.

**Check facts.** The checker needs a few facts about the clip. For a text request they are read
from the request; these options override them:

| Option | Read from the request | Checks that use it |
|---|---|---|
| `--duration SECONDS` | the first length (`6 s`, `<6> s`, `10-second`, `15s`) | shot times inside the real length, the FL2VA seconds |
| `--refs N` | the highest `Ref N` / `Picture N` (f2v: 1, or 2 with a last frame) | `<Picture N>` numbers, unused pictures |
| `--pinned[=BOOL]` | r2v: `Ref 1 is the first frame` (off when not mentioned) | keyframe tags and first-frame lines |
| `--last-frame[=BOOL]` | f2v: `Picture 2` or `last frame` (off when not mentioned) | I2VA vs FL2VA first line |

`--pinned` alone means true; write `--pinned=false` to say it is off (also `yes`/`no`, `1`/`0`).
In f2v, `--refs 2` implies a last frame and `--last-frame` implies 2 pictures. A spec file gives
all facts exactly. `lint_context` in the JSON shows the facts that were used.

The request is read the way the templates write it, so other numbers can be misread (an age
such as `in her 20s`, `3 sisters`). Scripts and agents should pass `--duration`, `--refs`,
`--pinned` and `--last-frame` explicitly when a request text is ambiguous, and check
`lint_context` with `--dry-run` first.

**Generation** (defaults: `writer.*` in the config - thinking off, temperature 0.4, max tokens
1500, top-p 0.95, top-k 20, min-p 0.05, random seed):

| Option | Meaning |
|---|---|
| `--thinking off\|low\|medium\|high` | Bonsai's reasoning. **Off** is tested best (valid prompts in 10-25 s); low/high burned 1-3k tokens without answering on the 8 GB laptop. Aliases: `none`, `xhigh`, `max`. |
| `--temperature T` | 0-2 |
| `--max-tokens N` | new tokens, reasoning included |
| `--top-p P`, `--top-k K`, `--min-p P` | sampling |
| `--seed N` | fixed seed for repeatable answers |

**Result:**

| Option | Meaning |
|---|---|
| `--no-history` | do not save the prompt (then `bh3 fix --last` fixes an older prompt, not this one) |
| `--strict` | exit 1 when the check finds errors (the prompt is printed anyway) |
| `--dry-run` | print what would be sent (request, check facts, settings) and exit 0; Bonsai is not used |

**Server and output:**

| Option | Meaning |
|---|---|
| `--no-auto-start` | exit 3 instead of starting Bonsai. Also off when `auto_start` is false in the config |
| `--timeout SECS` | how long to wait for a start (default `start_timeout_secs`, 300) |
| `--stop-after` | stop Bonsai afterwards **if this command started it**, also after a failure (frees the GPU for ComfyUI). A Bonsai that was already running is left alone |
| `--stream` | print the reasoning and the raw answer to stderr while Bonsai writes |
| `--out FILE` | also save the prompt to FILE: UTF-8 with BOM and CRLF, like Save .txt in the app (exit 1 when it cannot be written) |
| `--copy` | also copy the prompt to the clipboard (a failure is only a warning) |

When Bonsai is not ready, `write` waits for a server that is loading, or starts one (not forced:
it refuses while other programs hold the GPU memory and tells you about `bh3 start --force`).

**Output.** Plain: the prompt on stdout; progress, speed and the check summary on stderr:

```text
Writing an r2v prompt (6 s = 6.58 s real, 2 pictures; thinking off, temperature 0.4)...
Bonsai is writing...
Done in 18.7 s: 1300 new + 672 cached prompt tokens, 482 generated at 31.4 tok/s.
Check: score 92 - 0 errors, 1 warning, 0 info (r2v, 6 s, 2 pictures)
  warning description-short (line 12): detailed_description has 157 words; aim for 350-500
Saved to the history as 20261028-121637-817. To fix what the check found: bh3 fix --id 20261028-121637-817 --from-lint
```

JSON (`--json`): the fields of the written result, the mode and what the run did:

```json
{
  "mode": "r2v",
  "prompt": "subject_definitions:\n<Subject 1> is the woman in <Picture 1>, ...",
  "reasoning": "",
  "lint": {
    "mode": "r2v",
    "ok": true,
    "score": 92,
    "word_count": 327,
    "description_words": 157,
    "shots": [{ "number": 1, "time_s": null, "words": 157 }],
    "issues": [
      {
        "severity": "warning",
        "code": "description-short",
        "message": "detailed_description has 157 words; aim for 350-500",
        "line": 12
      }
    ],
    "errors": 0,
    "warnings": 1,
    "infos": 0
  },
  "lint_context": {
    "mode": "r2v",
    "duration_s": 6.0,
    "ref_count": 2,
    "first_frame_pinned": false,
    "has_last_frame": null
  },
  "result": {
    "content": "subject_definitions:\n...",
    "reasoning": "",
    "finish_reason": "stop",
    "timings": {
      "prompt_n": 1300,
      "cache_n": 672,
      "prompt_ms": 3299.6,
      "prompt_per_second": 393.99,
      "predicted_n": 482,
      "predicted_ms": 15311.73,
      "predicted_per_second": 31.41
    },
    "usage": { "prompt_tokens": 1972, "completion_tokens": 482, "total_tokens": 2454, "cached_tokens": 672 },
    "elapsed_ms": 18710,
    "cancelled": false
  },
  "history_id": "20261028-121637-817",
  "parent_id": null,
  "started_server": false,
  "stopped_server": false,
  "out_file": null,
  "copied": false,
  "warnings": []
}
```

- `prompt`: the cleaned prompt, ready for ComfyUI. `result.content` is Bonsai's raw answer.
- `lint`: the check report (`ok` = no errors; score = 100 - 25 per error - 8 per warning - 2 per
  info). See [Check codes](#check-codes).
- `history_id`: `null` with `--no-history` (or when saving failed: then `warnings` says so).
- `parent_id`: the fixed entry (`fix` only).
- `started_server` / `stopped_server`: this command started / stopped Bonsai.
- `warnings`: problems that did not stop the command (token limit reached, clipboard, a failed
  stop, `--out` failed).

`--dry-run --json`:

```json
{
  "mode": "r2v",
  "request": "Ref 1: a young woman in a red coat. ...",
  "parent_id": null,
  "lint_context": { "mode": "r2v", "duration_s": 6.0, "ref_count": 2, "first_frame_pinned": false, "has_last_frame": null },
  "params": { "thinking": "off", "temperature": 0.4, "top_p": 0.95, "top_k": 20, "min_p": 0.05, "max_tokens": 1500, "seed": null, "stream": true },
  "images": 0,
  "messages": 2,
  "save_history": true
}
```

## describe

```text
bh3 describe <IMAGE> [--role ROLE] [--no-cache] [options]
```

Bonsai looks at one picture with its vision input (the server needs its `*mmproj*.gguf`) and
writes **one compact paragraph** of what it shows for the picture's role, in the plain visual words
the requests use. Use it as the Ref's description: the prompt is then written from text, which is
faster and cache-friendly (a picture sent with every prompt costs ~1,100 tokens and ~18 s each
time). It is what the app does when you pick a picture for a reference.

| Option | Meaning |
|---|---|
| `IMAGE` | the picture: png, jpg, webp, gif or bmp (bigger than 1024 px: scaled down before sending) |
| `--role ROLE` | what the picture gives the video, and so what is described [default: `person`]: `person` / `same-person` (age range, face, hair, skin, build, clothing with colours and materials, accessories, pose), `place` (setting, light, time of day, key objects, colours, mood; no people), `person-and-place`, `outfit` (the garments only), `prop` (the object only), `style` (colour grade, light, lens and film look; not the content), `first-frame` / `last-frame` / `keyframe` (exactly what the frame shows: who, pose, framing, camera angle, setting), `storyboard`. Also accepted: `background`, `object`, `look`, `first`, `last`, and `_` instead of `-` |
| `--no-cache` | ask Bonsai again even when this picture and role were described before |
| `--no-auto-start`, `--timeout`, `--stop-after`, `--stream`, `--out`, `--copy` | as for `write` |

- Settings of the request: thinking off, temperature 0.2, at most 220 new tokens; a fixed system
  prompt (cached by llama-server), the role's instruction and the picture.
- The answer is cleaned: quotes, markdown, list marks and lead-ins such as "This image shows" are
  removed, so it starts with the subject ("a young woman in her twenties with ..."). A paragraph
  cut at the token limit loses its unfinished last sentence (a warning says so).
- **Cache:** `<config folder>\descriptions\<sha256 of the file>-<role>.json`
  (`{"description", "model", "created"}`). The same picture (same bytes, any file name) and role
  is answered from it at once, without Bonsai; another role is described again.
- A server without a vision projector is exit 1: "this Bonsai has no vision projector (mmproj),
  so it cannot see pictures - describe the picture in words". Bonsai not running and not started:
  exit 3. A missing file or one that is not a picture: exit 2.

The description goes to stdout. With `--json`:

```json
{
  "path": "D:\\refs\\cafe.jpg",
  "role": "place",
  "description": "an empty corner cafe with tall windows, light oak tables, rattan chairs and a white marble counter; soft morning daylight, warm beige and green tones, a calm quiet mood.",
  "cached": false,
  "model": "Bonsai-2-27B-PTQ1_0.gguf",
  "created": "2026-10-28 14:03:12",
  "elapsed_ms": 17950,
  "timings": { "prompt_n": 1210, "cache_n": 0, "predicted_n": 64, "predicted_per_second": 31.2 },
  "finish_reason": "stop",
  "started_server": false,
  "stopped_server": false,
  "out_file": null,
  "copied": false,
  "warnings": []
}
```

`cached: true` has `elapsed_ms: 0`, no `timings` and `finish_reason: "cache"`.

**In a request form:** `bh3 write --spec-file form.json --describe` first describes every used ref
that has an `image_path`, an empty `description` and `send_image` off (cache first, one Bonsai
request per new picture), then writes the prompt from the filled-in form. With `--dry-run` only
cached descriptions are filled in. `--describe` needs `--spec-file`.

## fix

```text
bh3 fix (--id ID | --last) (--fix TEXT | --from-lint) [options]
```

Sends a follow-up fix for a saved prompt: Bonsai gets the same system prompt, the entry's request,
its prompt and then the fix, so the cached system prompt is reused and a fix costs only a few
hundred new tokens. The result is saved as a new history entry whose `parent_id` is the fixed one.

| Option | Meaning |
|---|---|
| `--id ID` / `--last` | the entry to fix (`bh3 history list`); `--last` is the newest |
| `--fix TEXT` | what to change. Tested texts: `bh3 fixes` (fill their `<...>` parts) |
| `--from-lint` | check the saved prompt again and send its errors and warnings as the fix ("Fix these problems and keep everything else the same: - ..."). Exit 1 when the check finds nothing to fix |

The check facts are the ones the entry was checked with (saved with it; for older entries its
form, or its request read as in `write`); `--duration`, `--refs`, `--pinned` and `--last-frame`
override them, and after "Make it 10 s ..." the new length is taken from the fix. The pictures a
form sent to Bonsai's vision input are sent again; one that cannot be read any more is a
warning, and the fix goes without the pictures. All generation, result and server options of
`write` work the same, `--dry-run` included (it prints the fix text that would be sent). The JSON
is the same as for `write`, with `parent_id` set.

```powershell
bh3 fix --last --fix "Make the detailed_description longer (about 350 words): add light, fabric, hair movement, small gestures and the camera." --json
bh3 fix --id 20261028-121637-817 --from-lint --dry-run
```

If an answer goes badly wrong, write it again instead of piling up fixes: every fix adds the
previous prompt to the conversation.

## lint

```text
bh3 lint [--mode r2v|f2v] [--file FILE] [--duration N] [--refs N] [--pinned[=BOOL]] [--last-frame[=BOOL]] [--json]
```

Checks a prompt against the official H3 format: sections and their order, task tags, picture
numbers, retention lines, shot timestamps against the real clip length, the f2v first line,
dialogue tags and description length. Exit 0 when there are no errors, 1 when there are.

- Without `--file` the prompt is read from stdin (a terminal or empty stdin is a usage error).
- Without `--mode` the mode is detected from the text (r2v when it cannot tell).
- The check facts are only what you pass: unknown facts skip their checks.

```text
r2v prompt: score 0 - 5 errors, 1 warning, 2 info (r2v, 15 s, 2 pictures)
231 words, description 87 words, 4 shots: [Shot 1], [Shot 2] at 5.000 s, [Shot 3] at 10.000 s, [Shot 4] at 13.000 s
  error markdown-headings: section headings are written in markdown (line 1): write each one as `subject_definitions:` on its own line, ...
  error single-take-with-shots (line 8): "single continuous take" but the description has 4 shots: ...
  error shot-time-format (line 19): [Shot 2] opens with "Around 00:05.000"; write "[Shot 2] At 00:05.000, ..."
  warning description-short (line 15): detailed_description has 87 words; aim for 350-500
  info unused-subject (line 1): <Subject 2> is defined but never appears in detailed_description
Not ready: fix the errors before rendering.
```

JSON: the check report with counts and the facts used:

```json
{
  "mode": "r2v",
  "ok": false,
  "score": 0,
  "word_count": 231,
  "description_words": 87,
  "shots": [{ "number": 1, "time_s": null, "words": 25 }, { "number": 2, "time_s": 5.0, "words": 18 }],
  "issues": [
    { "severity": "error", "code": "shot-time-format", "message": "[Shot 2] opens with \"Around 00:05.000\"; write \"[Shot 2] At 00:05.000, ...\"", "line": 19 }
  ],
  "errors": 5,
  "warnings": 1,
  "infos": 2,
  "context": { "mode": "r2v", "duration_s": 15.0, "ref_count": 2, "first_frame_pinned": null, "has_last_frame": null }
}
```

Issues are sorted errors first, then by line. `line` is 1-based or `null`.

## chat

```text
bh3 chat (--message TEXT | --message-file FILE | --stdin) [--system none|r2v|f2v|file:PATH] [--image PATH]... [options]
```

One message to Bonsai, like its web UI. `--system r2v` / `f2v` uses the H3 prompt writer's system
prompt, `file:PATH` your own. Generation defaults are `chat.*` in the config (thinking off,
temperature 0.7, max tokens 2048). The server and output options of `write` work the same
(`--no-auto-start`, `--timeout`, `--stop-after`, `--stream`, `--out`, `--copy`). Chats are not
saved.

Plain output: the answer on stdout. JSON: the answer and the run facts:

```json
{
  "content": "Blue",
  "reasoning": "",
  "finish_reason": "stop",
  "timings": { "prompt_n": 17, "cache_n": 0, "prompt_ms": 120.5, "prompt_per_second": 141.1, "predicted_n": 2, "predicted_ms": 124.2, "predicted_per_second": 16.1 },
  "usage": { "prompt_tokens": 17, "completion_tokens": 2, "total_tokens": 19, "cached_tokens": 0 },
  "elapsed_ms": 1320,
  "cancelled": false,
  "started_server": false,
  "stopped_server": false,
  "out_file": null,
  "copied": false,
  "warnings": []
}
```

An empty answer is exit 4 (e.g. thinking used all tokens: the message says so).

## Library: templates, template, fixes, system-prompt, guide

The texts behind the writer, including your edits from the app's Library workspace.

| Command | Prints | JSON |
|---|---|---|
| `bh3 templates [--mode r2v\|f2v]` | the request templates: number, id, mode, title | `[{"number", "id", "title", "mode", "text"}]` |
| `bh3 template <ID>` | one template's text (id or number) | `{"id", "title", "mode", "text"}` |
| `bh3 fixes [--mode r2v\|f2v]` | the tested follow-up fixes, numbered; with `--mode` only those that suit that mode | `["Make the detailed_description longer ...", ...]` |
| `bh3 system-prompt <r2v\|f2v>` | the system prompt Bonsai gets | `{"mode", "item", "overridden", "official_guide": null, "text"}` |
| `bh3 guide <r2v\|f2v>` | the short readable H3 guide | `{"mode", "item", "overridden", "official_guide", "text"}` |

`overridden` is true when the text is your edited version. Template ids: `1-photo-of-a-person`,
`several-photos-of-the-same-person`, `person-place-photo`, `one-photo-gives-the-person-and-the-place`,
`two-or-more-people`, `a-person-without-a-photo-joins-the-scene`, `outfit-or-prop-from-another-photo`,
`style-mood-from-a-photo`, `ref-1-is-the-exact-first-frame`, `with-dialogue` (r2v) and
`first-frame-only`, `first-last-frame` (f2v). An unknown id is exit 1 and lists them.

## history

Every prompt `write` and `fix` write is saved as `history\<id>.json` (unless `--no-history` or
`save_history` is false). Ids are local times such as `20261028-121637-817`.

### history list

```text
bh3 history list [--limit N] [--mode r2v|f2v] [--search WORDS] [--starred]
```

Newest first, 20 by default (`--limit 0` = all). `--search` keeps entries containing all the
words (title, request, prompt, id).

```text
ID                   DATE              MODE  SCORE  ERR  TITLE
20261028-121901-477  2026-10-28 12:19  r2v      98    0  Fix: Make the detailed_description longer (about 350 words)...
20261028-121637-817  2026-10-28 12:16  r2v      92    0  Ref 1: a young woman in her late twenties with...
```

JSON:

```json
[
  {
    "id": "20261028-121901-477",
    "created": "2026-10-28T12:19:01.477+05:45",
    "date": "2026-10-28 12:19",
    "mode": "r2v",
    "title": "Fix: Make the detailed_description longer (about 350 words)...",
    "lint_score": 98,
    "lint_errors": 0,
    "duration_s": 6,
    "starred": false,
    "parent_id": "20261028-121637-817",
    "fix": "Make the detailed_description longer (about 350 words): add light, fabric, hair movement, small gestures and the camera.",
    "words": 412,
    "tokens_per_second": 31.5
  }
]
```

### history show

```text
bh3 history show <ID|last> [--field prompt|request|fix|reasoning|json]
```

Without `--field`: a readable view (facts, request, fix, reasoning, prompt). With `--field`: only
that part, e.g. `--field prompt` for the prompt alone. `request` is the original request without
the fix of a follow-up; `fix` is the follow-up's fix (empty otherwise).

With `--json` (or `--field json`): the whole entry as saved:

```json
{
  "id": "20261028-121637-817",
  "created": "2026-10-28T12:16:37.817+05:45",
  "mode": "r2v",
  "title": "Ref 1: a young woman in her late twenties with...",
  "request": "Ref 1: ...",
  "output": "subject_definitions:\n...",
  "reasoning": "",
  "params": { "thinking": "off", "temperature": 0.4, "top_p": 0.95, "top_k": 20, "min_p": 0.05, "max_tokens": 1500, "seed": null, "stream": true },
  "timings": { "prompt_n": 1300, "cache_n": 672, "prompt_ms": 3299.6, "prompt_per_second": 393.99, "predicted_n": 482, "predicted_ms": 15311.73, "predicted_per_second": 31.41 },
  "lint_score": 92,
  "lint_errors": 0,
  "duration_s": 6,
  "parent_id": null,
  "spec": null,
  "starred": false
}
```

A follow-up's `request` holds the original request, `\n\nFollow-up fix: ` and the fix.
`--json --field prompt` gives `{"id": "...", "prompt": "..."}`.

### history export / delete

```text
bh3 history export <ID|last> <FILE>     # the prompt as .txt: UTF-8 with BOM, CRLF (Notepad, ComfyUI)
bh3 history delete <ID>                 # the id itself; `last` is refused
```

JSON: `{"id": "...", "file": "..."}` and `{"deleted": "..."}`. A missing entry is exit 1.

## lengths

```text
bh3 lengths [SECONDS...]
```

H3 renders 24 fps on a 17k+5 frame grid, so the requested length is rounded up and prompts must
use the real length. Without arguments: 3, 4, 5, 6, 8, 10, 12, 15 and 20 s.

```text
Requested  Frames  Real length  Max cuts  Max size (8 GB laptop)
      3 s      73       3.04 s         1  0.5 MP
      6 s     158       6.58 s         2  0.5 MP
     10 s     243      10.13 s         3  0.4 MP
     15 s     362      15.08 s         4  0.3 MP
```

JSON: `[{"seconds": 6.0, "frames": 158, "real": "6.58", "real_s": 6.58, "max_cuts": 2, "max_megapixels": 0.5}]`.
`max_cuts` is the most cuts that still give each shot time to play out; `max_megapixels` the
largest output size that rendered reliably on the 8 GB laptop.

## tokens

```text
bh3 tokens (--text TEXT | --file FILE | < stdin)
```

Counts tokens with Bonsai's tokenizer. Needs Bonsai running (exit 3 otherwise; it is not started
for this). Plain: the number. JSON: `{"tokens": 131, "chars": 516}`.

## config

```text
bh3 config show               # all settings (the whole config.json with --json)
bh3 config path               # the config.json path (JSON: {"path", "dir", "exists"})
bh3 config get <KEY>          # one value (JSON: {"key", "value"})
bh3 config set <KEY> <VALUE>  # change one value (JSON: {"key", "value"})
```

The app and `bh3` share these settings (close the app before changing them, or it may save its
own copy over yours). A config.json that cannot be read is reported (exit 1), never overwritten.

| Key | Values | Default | Meaning |
|---|---|---|---|
| `bonsai_dir` | folder, or `none` | not set | the Bonsai-demo folder (`bh3 install set` checks it first); not used while `runtime.model` is set |
| `runtime.model` | file, or `none` | not set | model `.gguf` started directly, where it is (set: no Bonsai-demo folder needed; `bh3 models use`) |
| `runtime.mmproj` | file, or `none` | not set | its vision projector (none = no image input) |
| `runtime.server_exe` | file, or `none` | not set | llama-server.exe (unset: the newest build in the app's storage, else `bonsai_dir`'s) |
| `runtime.dir` | folder, or `none` | `%LOCALAPPDATA%\BonsaiH3Writer\runtime` | storage of downloads and imports |
| `runtime.models_dir` | folder, or `none` | `<runtime.dir>\models` | where models are downloaded / imported to |
| `runtime.hf_token` | token, or `none` | not set (`HF_TOKEN` is used) | Hugging Face token for gated / private repos; shown as `***` |
| `server_url` | URL | `http://127.0.0.1:8080` | llama-server address |
| `auto_start` | true/false | true | start Bonsai when `write` / `fix` / `chat` need it |
| `auto_stop_idle_minutes` | number | 10 | the app stops Bonsai after this many idle minutes if it started it (0 = never) |
| `stop_on_exit` | true/false | true | the app stops Bonsai when it closes, if it started it |
| `vram_busy_mb` | number | 1500 | refuse a start (not forced) while other programs use more VRAM (0 = never refuse) |
| `ctx_size` | number | 0 | context size for the launcher (`BONSAI_CTX`, 0 = launcher default) |
| `mmproj_cpu` | true/false | false | vision projector in system RAM (`BONSAI_MMPROJ_CPU=1`, frees ~0.9 GB VRAM) |
| `gpu_layers` | number, or `auto` | auto | layers on the GPU (`BONSAI_NGL`; auto = all, 0 for the CPU build) |
| `image_max_tokens` | number, or `auto` | auto | image token cap (`BONSAI_IMAGE_MAX_TOKENS`; auto = none with CUDA, 1024 otherwise; 0 = no cap) |
| `kv_cache_4bit` | true/false | false | q4_0 KV cache (`BONSAI_KV4=1`), ~3.5x less context memory |
| `extra_args` | text | empty | extra llama-server arguments, last on its command line (quotes group words) |
| `start_timeout_secs` | number | 300 | how long to wait for the model to load |
| `default_duration` | 1-120 | 6 | the app's default clip length |
| `save_history` | true/false | true | save written prompts |
| `auto_describe` | true/false | true | the app describes a picture picked for a reference once with Bonsai (see [describe](#describe)) |
| `writer.thinking` | off/low/medium/high | off | prompt writing: thinking |
| `writer.temperature` | 0-2 | 0.4 | prompt writing: temperature |
| `writer.max_tokens` | number | 1500 | prompt writing: max new tokens |
| `writer.top_p` / `writer.min_p` | 0-1 | 0.95 / 0.05 | prompt writing: sampling |
| `writer.top_k` | 0-1000 | 20 | prompt writing: sampling |
| `writer.seed` | number, or `none` | none | prompt writing: fixed seed |
| `chat.*` | as `writer.*` | thinking off, temperature 0.7, max tokens 2048 | chat defaults |

Booleans also take `yes`/`no`, `on`/`off`, `1`/`0`. Keys ignore case, and `writer_params.` /
`chat_params.` work as prefixes too. An unknown key or a bad value is exit 2.

## install

```text
bh3 install detect        # Bonsai-demo folders found on this PC (D:\, C:\, E:\, your user folder, ...)
bh3 install set <DIR>     # check DIR and save it as bonsai_dir (exit 1 when it is not a Bonsai-demo folder)
bh3 install show          # the folder in use and what was found in it (exit 1 when it is not usable)
```

With `runtime.model` set (see [models](#models)), `install show` describes the app's runtime instead
(`"source": "runtime"`) and `install set` warns that the runtime takes precedence.

A Bonsai-demo folder has `scripts\start_llama_server.ps1` and/or `bin\<cuda|hip|vulkan|cpu>\llama-server.exe`;
it is usable with a `llama-server.exe` and a model (`*.gguf`) under `models\`. JSON: `detect` gives
`{"configured": "...", "installs": [...]}`, `set` and `show` one install object (as in `status`).

## models

Bonsai without the Bonsai-demo folder: the app starts PrismML's `llama-server.exe` directly with
the flags `start_llama_server.ps1` passes for a Bonsai 2 model. Files you choose stay where they
are (only their paths are saved); downloads and imports go to the app's storage (`runtime.dir`,
models to `runtime.models_dir`).

```text
bh3 models list                                     # models in the app's folder + the one in use, and the catalog
bh3 models use <ID | FILE.gguf | none> [--mmproj FILE|none] [--server EXE]
bh3 models download <ID | hf:OWNER/REPO/FILE.gguf> [--mmproj FILE|none] [--use]
bh3 models import <BONSAI_DEMO_DIR> [--move] [--no-use]
bh3 models remove <ID> [--force]
```

- `use` sets `runtime.model` (and `runtime.mmproj`: the `*mmproj*.gguf` next to the model unless
  `--mmproj` says otherwise; `none` = no image input) and, with `--server`, `runtime.server_exe`.
  `<ID>` is a model id from `list` (file name without `.gguf`, or a catalog id); any existing
  `.gguf` path works too and is used in place. `use none` switches back to `bonsai_dir`. Without a
  known llama-server it warns and names the build `bh3 runtime download` would get. Applies on the
  next start of Bonsai. JSON: `{"model", "mmproj", "server_exe", "bonsai_dir", "install"}`.
- `download` takes a catalog id - `27b-ptq1` (default for 8 GB GPUs, 5.5 GB + 600 MB projector),
  `27b-pq2`, `9b-ptq1`, `4b-ptq1`, `2b-ptq1` - or any Hugging Face file as `hf:owner/repo/file.gguf`
  (also a pasted `https://huggingface.co/.../resolve/main/...` link), with an optional projector of
  the same repo (`--mmproj`). Sizes and sha256 come from the catalog or the repo's file list; a
  file is renamed into place only when its checksum matches. `--use` uses it afterwards.
- `import` copies the Bonsai-demo folder's llama-server build (`bin\<backend>\llama-server.exe` and
  its DLLs, no other tools), the model the launcher would pick and its projector, and
  `scripts\webui-config.json` into the app's storage, then uses them (`--no-use`: only copy).
  `--move` moves them out of the folder instead. Files already in the app are skipped.
- `remove` deletes a model of the app's models folder (and its projector when no other model there
  uses it). Files outside that folder are never deleted (exit 2: not found there). The model in use
  needs `--force` (the setting is then cleared).

Downloads print progress on stderr (one updated line on a terminal, else a line every 5 s:
`Downloading <file> (1/2): 42% 2.3 GB of 5.5 GB · 38.0 MB/s · 1m 25s left`). The first Ctrl+C
cancels (exit 130) and keeps `<file>.part`: the same command continues where it stopped (HTTP
Range). Errors are clear: `not found (404)`, `access denied (401/403) ... set a Hugging Face token`,
`not enough disk space ...`, `checksum mismatch ...` (the damaged file is deleted). JSON of
`download` / `import`: `{"task", "server_exe", "model", "mmproj", "notes", "message", "used",
"install"}`.

## runtime

```text
bh3 runtime show                                          # server build in use, builds in the app, the build for this PC, storage
bh3 runtime download [--backend cuda-13.3|cuda-12.4|vulkan|cpu] [--no-use]
```

The server builds come from PrismML's llama.cpp fork, release `prism-b10787-73bfe6c` on GitHub
(the Bonsai 2 formats PTQ1_0 / PQ2_0 do not load in stock llama.cpp). Without `--backend` the build
is picked like the demo's `setup.ps1`: `nvidia-smi` reports CUDA 13.3 or newer -> `cuda-13.3`
(release zip + CUDA runtime zip, ~545 MB), 12.4 or newer -> `cuda-12.4` (~675 MB), else `vulkan`
when `vulkaninfo` is on PATH (CPU + Vulkan zips), else `cpu` (~19 MB, slow); without an NVIDIA
driver the reason says so. Only `llama-server.exe` and its DLLs are kept, in
`<runtime.dir>\bin\<release>-<backend>\`, and it is used (`runtime.server_exe`) unless `--no-use`.
If Windows lacks the Visual C++ runtime the build needs (`VCRUNTIME140.dll`, `MSVCP140.dll`,
`VCOMP140.DLL`), `download`, `show` and `status` name the missing DLLs and link to
https://aka.ms/vs/17/release/vc_redist.x64.exe.

New PC without the Bonsai-demo folder:

```powershell
bh3 models use "E:\Models\Ternary-Bonsai-2-27B-Uncensored-Heretic-PTQ1_0.gguf"
bh3 runtime download
bh3 status            # exit 3 and install.problems while something is missing
```

## gpu, logs, agent-guide, version

| Command | Prints |
|---|---|
| `bh3 gpu` | GPU memory, temperature, load, the processes on the GPU and, while no llama-server runs, whether a start would be refused. JSON: the `gpu` object of `status`. Exit 1 without `nvidia-smi` |
| `bh3 logs [--lines N]` | the last N lines (default 50) of `server.log`, written when `bh3` or the app starts Bonsai. JSON: `{"path", "exists", "lines": [...]}` |
| `bh3 agent-guide` | the guide for LLM agents ([AGENT_GUIDE.md](AGENT_GUIDE.md)). JSON: `{"guide": "..."}` |
| `bh3 version` | `bh3 0.1.0 (Bonsai H3 Writer)`. JSON: `{"name", "app", "version"}` |

## mcp

```text
bh3 [--url URL] [--bonsai-dir DIR] mcp
```

Runs a Model Context Protocol server on stdin/stdout for AI agents: newline-delimited JSON-RPC
2.0, stdout carries only protocol messages, notes go to stderr. MCP clients start it themselves,
e.g. `claude mcp add bonsai-h3 -- "C:\path\to\bh3.exe" mcp`. Its tools (`write_prompt`,
`fix_prompt`, `lint_prompt`, `bonsai_status`, `start_bonsai`, `stop_bonsai`, ...) do what the
commands do; only `stop_bonsai` stops Bonsai. It runs until stdin closes (exit 0). The settings
are read again for every call. Setup, tools and arguments: [MCP.md](MCP.md).

## Request spec files

`write --spec-file FILE` takes the app's request form as JSON. Every field is optional; missing
fields get these defaults:

```json
{
  "mode": "r2v",
  "refs": [
    { "role": "person", "description": "a young woman with short black hair in a denim jacket", "image_path": null, "send_image": false },
    { "role": "place", "description": "a rainy neon street at night" }
  ],
  "duration_s": 6,
  "cuts": 0,
  "action": "she turns to the camera and smiles",
  "place": "",
  "outfit": "",
  "style": "film35mm",
  "style_custom": "",
  "camera": "auto",
  "camera_custom": "",
  "dialogue": [{ "speaker": "she", "words": "Good evening." }],
  "extra_people": "",
  "notes": "",
  "first_frame_pinned": false
}
```

| Field | Values |
|---|---|
| `mode` | `r2v`, `f2v` |
| `refs[].role` | `person`, `same_person`, `place`, `person_and_place`, `outfit`, `prop`, `style`, `first_frame`, `last_frame`, `keyframe`, `storyboard` (f2v: Picture 1 is the first frame; `last_frame` marks Picture 2) |
| `refs[].image_path`, `refs[].send_image` | a picture file and whether to send its pixels to Bonsai with the prompt (relative paths are relative to the spec file and saved to the history as absolute paths); `write --describe` fills an empty `description` from the picture instead |
| `refs[].auto_description` | set by the app / `--describe`: the description Bonsai wrote (while `description` equals it, it counts as Bonsai's text and may be replaced by a new description) |
| `duration_s`, `cuts` | requested seconds; 0 cuts = one take |
| `style` | `film35mm`, `natural`, `documentary`, `commercial`, `noir`, `anime`, `custom` (then `style_custom`) |
| `camera` | `auto`, `static`, `push_in`, `pull_out`, `tracking`, `arc`, `handheld`, `close_side_view`, `custom` (then `camera_custom`) |
| `first_frame_pinned` | r2v: Ref 1 is the exact first frame (First-frame pin on) |

r2v uses up to 9 refs, f2v up to 2. The form becomes a request text like the templates (see it
with `--dry-run`), and its facts drive the check. Problems such as an empty action are printed as
notes; a picture to send that cannot be read is exit 1.

## Check codes

Codes are stable: scripts and agents can match on them. Severity `error` breaks the H3 format
(fix before rendering), `warning` is likely to hurt the video, `info` is a hint from the guides.

| Code | Severity | Mode | Meaning |
|---|---|---|---|
| `empty-prompt` | error | both | nothing to check |
| `wrong-mode` | error | both | the text is a prompt of the other mode |
| `internal` | error | both | the checker itself could not run |
| `markdown-headings` | error | both | `**summary**` / `### summary` instead of `summary:` (sections are still parsed) |
| `missing-section` | error | both | a required section / field is missing |
| `duplicate-section` | error | both | a section appears twice |
| `section-order` | error | both | sections are not in the required order |
| `empty-section` | error | both | a section has no text |
| `text-before-sections` | warning | both | lead-in text before the first section (f2v: besides line 1) |
| `text-after-sections` | warning | both | notes after the last section, separated by a blank line |
| `missing-task-tag` | error | r2v | summary does not start with a `[task]` tag |
| `bad-task-tag` | error | r2v | unknown, repeated or video/audio-only task tag |
| `keyframe-without-frame` | error | r2v | keyframe tag, `is the first frame` or `begins from <Picture N>` with the First-frame pin off |
| `pinned-frame-not-used` | warning | r2v | Ref 1 is pinned but the tag / first-frame line / "begins from" is missing |
| `first-frame-picture` | error | r2v | the first frame is a picture other than `<Picture 1>` |
| `keyframe-tag-unused` | warning | r2v | `[keyframe completion]` but no picture is named as a frame |
| `picture-out-of-range` | error | both | `<Picture N>` above the loaded pictures (or 0, or above 9 / 2) |
| `unused-picture` | warning | both | a loaded picture is never cited |
| `no-subjects` | error | r2v | subject_definitions defines no `<Subject N>` |
| `undefined-subject` | error | r2v | `<Subject N>` used but not defined |
| `unused-subject` | info | r2v | a defined subject never appears in detailed_description |
| `missing-retention` | warning | r2v | a defined label has no retention_analysis line |
| `retention-format` | warning | r2v | not `<Subject N> (appears in [Shot 1]): marker - ...` |
| `bad-retention-marker` | error | r2v | marker is not fully_preserved / partially_preserved / attribute_transfer / weak_reference |
| `speaker-id-in-retention` | error | r2v | `(S1)` ids in retention_analysis |
| `unknown-shot` | warning | r2v | `[Shot N]` cited but the description has no such shot |
| `shot-count-mismatch` | warning | r2v | the summary's shot count differs from the description |
| `missing-style-sentence` | warning | r2v | no style sentence before `[Shot 1]` |
| `style-sentence-start` | info | r2v | the style sentence does not start "The target video is" |
| `missing-shot-1` | error | both | no `[Shot 1]` in the description |
| `f2v-description-start` | warning | f2v | integrated_multimodal_description does not start with `[Shot 1]` |
| `shot-numbering` | error | both | shots are not numbered 1, 2, 3 ... |
| `shot1-time` | error | both | `[Shot 1]` has a time |
| `shot-time-missing` | error | both | a later shot has no time |
| `shot-time-format` | error | both | a later shot's time is not `At MM:SS.mmm,` (e.g. `Around 00:05.000`) |
| `shot-time-order` | error | both | shot times do not increase |
| `shot-time-past-end` | error | both | a shot time is at or past the real clip length |
| `short-shot` | info | both | a shot lasts under 1 s |
| `many-cuts` | info | both | more cuts than the length suggests (`bh3 lengths`, max cuts) |
| `time-inside-shot` | warning | both | a time inside a shot ("At 00:01.200, she ...") |
| `cut-without-shot` | warning | both | "cuts to" inside a shot instead of a new `[Shot N]` |
| `single-take-with-shots` | error | both | "single continuous take" / "no cuts" / "one take" but more than one shot |
| `description-short` | warning | both | description under 200 words (f2v: 100) |
| `description-below-target` | info | both | under 350 words (f2v: 150) |
| `description-long` | info | both | over 600 words (f2v: 400) |
| `no-subtitles-line` | info | both | "No subtitles, captions, text, watermarks or logos." is missing |
| `soundscape-long` | info | both | overall_soundscape over 4 sentences |
| `music-long` | info | both | non_diegetic_music over 3 sentences |
| `dialogue-in-soundscape` | warning | both | speech in overall_soundscape |
| `dialogue-unclosed` | error | both | `<d>` without `</d>` |
| `dialogue-language` | warning | both | `<d>` without a `[Language]` tag |
| `dialogue-speaker` | warning | both | `<d>` without a `(S1)` id before it |
| `dialogue-not-tagged` | warning | both | quoted speech (`she says "..."`) outside `<d>` |
| `f2v-words-in-r2v` | error | r2v | an f2v field or alignment line in an r2v prompt |
| `r2v-words-in-f2v` | error | f2v | `<Subject`, subject_definitions, summary: ... in an f2v prompt |
| `f2v-line1-missing` | error | f2v | line 1 is not the I2VA / FL2VA alignment line |
| `f2v-line1-format` | error | f2v | line 1 is not exactly the I2VA / FL2VA form |
| `f2v-line1-mismatch` | error | f2v | I2VA with a last frame, or FL2VA without one |
| `f2v-line1-blank` | warning | f2v | no blank line after line 1 |
| `f2v-line1-dash` | info | f2v | the FL2VA line has no em dash |
| `fl2va-length` | error | f2v | FL2VA seconds differ from the real length of the duration (`bh3 lengths`) |
| `fl2va-shot` | error | f2v | FL2VA `Picture 2 (from Shot N)` is not the last shot |
| `fl2va-ending` | warning | f2v | with a last frame, the description never reaches Picture 2 |

## Text encoding on Windows

H3 prompts contain non-ASCII characters (the FL2VA line has an em dash). `bh3` writes UTF-8 to
pipes and files, and the console shows it correctly. PowerShell, Windows PowerShell 5.1 and
PowerShell 7 alike, decodes the output it captures from programs (`$r = bh3 ...`, `bh3 ... |
ConvertFrom-Json`) with `[Console]::OutputEncoding`, which is the console code page (437, 850,
...) unless Windows' system-wide UTF-8 option is on. So in PowerShell either use the file options
(`--out`, `history export`, `--request-file`) or switch to UTF-8 first:

```powershell
[Console]::OutputEncoding = [Text.UTF8Encoding]::new()
$OutputEncoding = [Text.UTF8Encoding]::new()      # for text piped *into* bh3 (PowerShell 7 has it already)
```

Input files may be UTF-8 (with or without BOM) or UTF-16 with BOM (what `>` writes in PowerShell
5.1). Git Bash and files written by cmd.exe redirection (`bh3 ... > out.txt`) keep the UTF-8
bytes as they are.

## Recipes

Write, check and copy in one go (PowerShell):

```powershell
[Console]::OutputEncoding = [Text.UTF8Encoding]::new()   # PowerShell 5.1 and 7: read bh3's output as UTF-8
$r = bh3 write --mode r2v --request-file req.txt --json --stop-after | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw $r.error }
if (-not $r.lint.ok) {
    $r = bh3 fix --id $r.history_id --from-lint --json --stop-after | ConvertFrom-Json
}
Set-Clipboard $r.prompt
```

The same in bash:

```bash
out=$(bh3 write --mode f2v --request-file req.txt --json) || { echo "$out" | jq -r .error; exit 1; }
[ "$(echo "$out" | jq .lint.ok)" = true ] || out=$(bh3 fix --last --from-lint --json)
echo "$out" | jq -r .prompt > prompt.txt
```

Check every prompt in a folder:

```powershell
Get-ChildItem prompts\*.txt | ForEach-Object {
    $report = bh3 lint --file $_.FullName --json | ConvertFrom-Json
    "{0}: score {1}, {2} errors" -f $_.Name, $report.score, $report.errors
}
```

Free the GPU before a ComfyUI render: `bh3 stop` (or `--stop-after` on the last `write` / `fix`).
