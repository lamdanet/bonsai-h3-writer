# bh3 - guide for LLM agents

`bh3` writes **MiniMax H3** video prompts with the user's local **Bonsai 27B** (llama-server) and
checks them against the official H3 format. Let Bonsai write the prompt and `bh3` check it; do not
hand-write or hand-edit H3 prompts. Full reference: `docs/CLI.md`, or `bh3 <command> --help`.

## Rules

- Pass `--json` and read stdout as one JSON document. Check the exit code first; on failure the
  JSON is `{"error": "...", "exit_code": N, "kind": "..."}`, except for these non-zero exits that
  print the normal result: `status` exit 3 (`.message` says why), `lint` and `write` / `fix
  --strict` exit 1 (the prompt has check errors), `install show` exit 1 (`.problems`), and a
  `--out` file that cannot be written (exit 1; the prompt was written and saved, the problem is in
  `.warnings`: do not write it again).
- Leave thinking **off** (the default). Never use `--thinking high` (or medium / low) on the user's
  8 GB laptop: thinking burned 1,000-3,000 tokens without answering. Off writes a valid prompt in
  10-25 s.
- Describe the pictures **in words** in the request. When the user gives a picture file, run
  `bh3 describe <file> --role person|place|outfit|style|first-frame|... --json` once (~18 s, cached
  afterwards) and put `.description` into the request as that picture's line. `--image` sends the
  picture with every prompt (~1,100 tokens and ~18 s each time) and is rarely better.
- Use `--stop-after` when the user renders in ComfyUI next: it stops Bonsai (frees ~7.7 GB of VRAM)
  if your command started it. A Bonsai that was already running is left alone.
- Do not run `bh3 stop`, `bh3 restart` or `bh3 start --force` unless the user asks: `stop` ends every
  `llama-server.exe` on the PC, and a forced start runs out of GPU memory while ComfyUI is open.
- Fix with follow-ups (`bh3 fix`), not by editing the prompt. If an answer is badly wrong (wrong
  mode, wrong people), write it again with `bh3 write` instead of piling up fixes.
- Keep the history on: `--no-history` means `bh3 fix --last` would fix an older prompt.

## Recommended flow

```text
bh3 status --json                                  # exit 0: ready. 3: not running (write starts it)
bh3 describe photo.jpg --role person --json        # only for picture files: .description -> "Ref 1: ..."
bh3 write --mode r2v --request-file req.txt --json --stop-after
  -> .lint.ok true?  done: give the user .prompt
  -> .lint.ok false: bh3 fix --last --from-lint --json --stop-after   (errors + warnings as the fix)
     or your own words: bh3 fix --last --fix "Make the detailed_description longer (about 350 words)." --json
bh3 history show last --field prompt               # the final prompt as plain text
```

- `.prompt` is the cleaned prompt for ComfyUI; `.history_id` names the saved entry
  (`bh3 fix --id <ID>` fixes that one); `.warnings` lists problems that did not stop the command.
- `.lint.issues[]` has `severity` (`error` / `warning` / `info`), a stable `code` and a `message`.
  Errors break the format; warnings hurt the video; info are hints. One fix round is usually enough.
- `--dry-run` shows what would be sent (the request or the fix, the check facts, the settings)
  without using Bonsai: use it to check a request or a `--from-lint` fix first.
- `bh3 fixes` lists tested fix texts (fill their `<...>` parts).

## Writing the request (req.txt, UTF-8)

Plain words, like the templates (`bh3 templates`, `bh3 template <ID>`):

```text
Ref 1: a young woman, long black hair in a high bun, cream crochet one-piece swimsuit.
Ref 2: an empty sunlit cafe with tall windows and oak tables (place). Use the place from Ref 2.
6 s, one take: she walks to the window table and sits down. Camera: the camera pushes in slowly.
Style: film look: 35mm, soft grain, warm grade, shallow depth of field.
She says "Good morning."
```

- **r2v** (reference pictures): `Ref 1` ... `Ref 9` in the Load Image order of the ComfyUI workflow.
  Say what each picture gives: person, place, outfit, prop or style. Write
  `Ref 1 is the first frame: ...` only when the user pins Ref 1 as the exact first frame.
- **f2v** (first frame): `Picture 1: <the first frame>`; add `Picture 2: <the last frame>` only for
  a first + last frame clip (FL2VA).
- Describe each picture: who (age, hair, face, figure), clothes, pose, place, light, framing. The
  same person in several photos: say so (`Ref 2: the same woman, side view`).
- Name the length (`6 s`) and the cuts (`one take`, `2 cuts`). One continuous action per shot,
  about one cut per 3 s at most (`bh3 lengths`). Dialogue: exact words in quotes.
- The checker reads the facts from the request: the length, the number of pictures,
  `Ref 1 is the first frame`, `Picture 2` / `last frame`. `.lint_context` shows what it used;
  `--duration`, `--refs`, `--pinned[=false]`, `--last-frame[=false]` override it. When the text
  is ambiguous (ages such as `in her 20s`, other numbers next to `s` or `Ref`), pass all four
  explicitly, e.g. `--duration 6 --refs 2 --pinned=false` (f2v: `--last-frame=false`), and check
  `.lint_context` with `--dry-run` first. A fix keeps the facts the entry was checked with.

## The H3 format in brief

- **r2v**: six sections, each `name:` on its own line (never markdown headings), in this order:
  `subject_definitions` (`<Subject N> is the woman in <Picture N>, ...`), `summary` (starts with
  `[reference generation]`; `[keyframe completion]` only for a real frame), `retention_analysis`
  (`<Subject 1> (appears in [Shot 1]): fully_preserved - ...`), `detailed_description` (350-500
  words, a style sentence first), `overall_soundscape`, `non_diegetic_music` (or `N/A`).
- **f2v**: line 1 is the exact I2VA line (`For the target video, at 0.00 seconds into the target
  video, <Picture 1> (from [Shot 1]) is fully referenced.`) or, with a last frame, the FL2VA line
  with the real length; a blank line; then `integrated_multimodal_description: [Shot 1] ...`,
  `overall_soundscape: ...`, `non_diegetic_music: ...`.
- Shots: `[Shot 1]` has no time; later shots open with `[Shot N] At MM:SS.mmm,`; times rise and stay
  inside the real length (6 s renders 6.58 s, see `bh3 lengths`).
- Speech: `<Subject 1> (S1) says, <d>[English] Good morning.</d>`.
- `bh3 guide r2v|f2v` prints the short guides, `bh3 lint --file F --json` checks any prompt.

## Exit codes

| Code | Meaning | What to do |
|---|---|---|
| 0 | ok | use the result |
| 1 | failure; `lint` / `--strict`: the prompt has errors; `--out` failed | read `.error`; `lint` / `--strict` / `--out` print the normal result (see Rules): fix the prompt with `bh3 fix`, or read `.warnings` |
| 2 | wrong arguments or empty input | fix the command (see `.error` and `--help`) |
| 3 | Bonsai unavailable | read `.error`: no Bonsai folder (`bh3 install detect`, `bh3 install set <DIR>`), GPU busy (ask the user to close ComfyUI), start failed (`bh3 logs`) |
| 4 | Bonsai answered with an error or nothing | retry once; raise `--max-tokens` if `.error` says so |
| 130 | cancelled with Ctrl+C | - |

## Commands at a glance

| Command | Use |
|---|---|
| `status` | is Bonsai ready? install, model, GPU |
| `write --mode r2v\|f2v --request-file F` | request -> prompt, checked and saved |
| `describe IMAGE --role ROLE` | a picture file -> one paragraph for its Ref line (cached; needs the vision projector) |
| `fix --last \| --id ID` + `--fix TEXT \| --from-lint` | follow-up change of a saved prompt |
| `lint [--file F]` | check any prompt (stdin without `--file`) |
| `templates`, `template ID`, `fixes [--mode r2v\|f2v]` | request templates and tested fix texts |
| `history list \| show ID\|last \| export ID FILE` | saved prompts |
| `lengths` | requested seconds -> frames, real length, max cuts, laptop size |
| `chat --message TEXT [--system r2v]` | a free question to Bonsai |
| `tokens`, `gpu`, `logs`, `config`, `install` | diagnostics and settings |
| `models list \| use FILE.gguf \| download ID`, `runtime show \| download` | Bonsai without the Bonsai-demo folder: model files in place, server build, downloads |
| `mcp` | the same functions as MCP tools (see below) |

## When Bonsai is not set up

`bh3 status --json` names what is missing in `install.problems` (exit 3). Without a Bonsai-demo
folder: `bh3 models use <file.gguf>` (the file stays where it is; a `*mmproj*.gguf` next to it is
found), then `bh3 runtime download` (the llama-server build for this GPU, ~545 MB for CUDA), then
write as usual. Do not download models (5-7 GB) unless the user asks for it. Downloads print
progress on stderr; Ctrl+C keeps the `.part` and the same command continues. A missing Visual C++
runtime is reported with Microsoft's link - the user has to install it.

## MCP

`bh3 mcp` is an MCP server on stdin/stdout, so an MCP client calls the writer as native tools
instead of running commands: `claude mcp add bonsai-h3 -- "<path>\bh3.exe" mcp`. Tools:
`bonsai_status`, `write_prompt`, `describe_image`, `fix_prompt`, `lint_prompt`, `clip_length`,
`get_guide`, `list_templates`, `list_history`, `get_history_entry`, `start_bonsai`, `stop_bonsai`,
`chat`. The rules above apply: thinking off, describe the pictures in words (`describe_image` for a
picture file), fix with `fix_prompt` (`from_lint:
true` or `fix`), give the user the prompt unchanged, and call `stop_bonsai` only when the user wants
the GPU back. A failed call is a result with `isError: true` and the reason. See `docs/MCP.md`.
