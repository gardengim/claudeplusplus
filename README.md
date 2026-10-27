# claude++

A small native (Rust) terminal frontend for **Claude Code running headless**.
The official `claude` binary still does everything agentic: model calls, tools,
permissions logic, sessions on disk. claude++ replaces only the UI and manages the
processes around it.

```
cargo build --release
./install.sh        # builds, installs ~/.local/bin/claude++ (and claudeplusplus, the fallback name)
claude++ --help
```

## Architecture

```
┌──────────── claude++ (one process, ~3 MB RSS) ────────────┐
│ main.rs     event loop, keys, slash commands, tabs, bench │
│ ui.rs       ratatui renderer (per-entry wrap cache)       │
│ session.rs  protocol → transcript state machine (per tab) │
│ backend.rs  child process + stream-json I/O               │
└───────┬───────────────────────────────┬───────────────────┘
        │ stdin/stdout NDJSON           │ (one per tab, lazily)
   claude -p --input-format stream-json --output-format stream-json
             --include-partial-messages --permission-prompt-tool stdio
```

- **One long-lived `claude -p` per tab.** stdin stays open, so a session is multi-turn
  without re-spawning. A message sent while a turn is running is queued by Claude Code.
- **Permissions and questions go over the control protocol.** `--permission-prompt-tool stdio`
  makes Claude Code send `control_request{subtype:"can_use_tool"}`. claude++ answers allow,
  allow-always (by echoing `permission_suggestions` back as `updatedPermissions`), or deny.
  `AskUserQuestion` comes through the same request, and the answers are returned in `updatedInput.answers`.
- **Protocol is handled as untyped JSON** (`serde_json::Value`). New or renamed upstream fields
  are ignored instead of breaking deserialization. Unknown control requests get an error reply,
  so the backend never hangs waiting on us.
- **Threads, no async runtime.** One stdin-reader thread for the terminal, plus one stdout
  reader per backend, all feed a single channel. The main loop drains a whole burst of
  streaming deltas before redrawing once.
- **Rendering is O(visible lines).** Each transcript entry caches its wrapped lines per width.
  Only the streaming entry is re-wrapped as text arrives, and only the visible window is
  handed to ratatui.

Dependencies: `ratatui` (with its bundled crossterm), `serde_json`, `unicode-width`.
Base64 (for images) and argument parsing are a few lines each, so they are hand-written.
Release binary: ~1.7 MB.

## What works (verified end-to-end against Claude Code 2.1.309)

- Streaming responses (token deltas) with a live activity line: requesting, thinking,
  calling a tool, elapsed time.
- Tool calls rendered Claude-Code style: `● Bash(cmd)`, `⎿` result preview, inline diffs for
  `Edit` and `Write`, nested subagent steps, and ctrl+o to expand everything.
- **Panels**: every interactive surface (prompts, suggestions, model picker, config, session browser) is drawn by
  one shared component at the bottom of the screen above the input, so they look and behave alike; see
  [DESIGN.md](DESIGN.md). **`/config`** edits claude++'s own config in such a panel: status line, profile for
  new tabs, inline images, prompt-cache lifetime, idle-suspend time and memory budget. ←→ changes a value, it
  applies at once and is saved to `~/.config/claude++/config.json`; a command-line flag still wins for that run.
- Prompts the agent waits on, as one choice-list panel (see [DESIGN.md](DESIGN.md)): permission requests (↑↓ + Enter,
  or y / a / n at once; plan approval shows the plan) and `AskUserQuestion` with tabs for several questions, a
  review step, checkboxes for multi-select, an inline "Other" answer (just start typing, or paste), and option
  previews. Esc declines. Pasted text can never act as a hotkey.
- Multi-turn conversations, Esc / ctrl+c interrupt (the process survives, and the next turn works).
- **Tabs**: independent sessions, each with its own backend. ctrl+t, ctrl+n / ctrl+p,
  alt+1..9, ctrl+d, `/new`, `/close`.
- **Image attachments**: ctrl+v pastes an image from the clipboard (macOS via AppleScript, elsewhere `wl-paste` or
  `xclip`); `/img path`, or dragging a png/jpeg/gif/webp into the terminal, attaches a file. Each picture is
  **inline text**: an `[Image #N]` token on its own line at the cursor, with a small preview drawn right under it
  (kitty/Ghostty), so it sits among your words. The message is sent in that order, each picture introduced by its
  label so "compare Image #1 and #2" works, and the transcript shows the same layout at a larger size. Backspace
  or Delete next to a token removes it whole (and drops the picture); deleting the text is how you un-attach.
  History recall keeps the words only. Images over the API's 5 MB limit are re-encoded smaller first (macOS `sips`). A terminal that sends an *empty* paste for an image-only
  clipboard is handled the same way as ctrl+v (not verified in Ghostty; ctrl+v is the reliable key).
- **`@` file picker**: type `@` (at the start of a word) for a popup of the project's files and folders: tracked
  and untracked-but-not-ignored files from git, else a bounded directory walk. Fuzzy match on the whole path
  (file name and word starts weigh most). Tab or Enter inserts it, a folder keeps the popup open for the next level, Esc hides it.
  Claude Code itself reads `@path` mentions in headless mode (text files as a read, images as images; verified),
  so claude++ only inserts the text, and additionally draws an `@image` inline in the transcript.
- **Model, effort and fast mode picker** (`/model` or `/effort` with no argument): the models Claude Code offers,
  with the effort levels each supports and, for Opus, a **fast mode** switch (space or `f`; same model, faster
  output, higher price per token, billed from usage credits; switching to a model without it turns it off, as in
  Claude Code). Fast mode is changed live with `apply_flag_settings {fastMode}` and the backend's reported state
  decides what the note says (`Fast mode ON ↯` or the reason it refused); the status line shows `↯` while it is on.
  Verified against the real backend without sending any request; a real Opus fast request was not made (it bills
  usage credits). ↑↓ model, ←→ effort, Enter applies live (`set_model` and `apply_flag_settings`, no restart),
  and the choice is re-applied if the tab's backend is suspended and resumed. The list comes from `initialize`
  and is cached next to the command list; the status line shows `model effort`. `/model <name>` and `/effort <level>` set directly.
  A model switch warns that prompt caches are per model.
- **Which status line is claude++'s own choice** (`/statusline claude | builtin`, remembered in
  `~/.config/claude++/config.json`; `--statusline claude|builtin|"<command>"` overrides without saving). `builtin`
  (the default) is claude++'s own single line and never spawns anything. `claude` runs your Claude Code status
  line: richer, at the price of a short-lived process per refresh (a Python script measured ~27 ms and ~17 MB each
  time). If `claude` is chosen but Claude Code has none configured, claude++ says so and shows its own line.
  The config file is a flat JSON object; unknown keys are kept when claude++ saves one. Customising the builtin
  line is not built yet.
- **Your Claude Code status line, run the way Claude Code runs it** (per its
  [status line docs](https://code.claude.com/docs/en/statusline)) when `claude` is chosen. claude++ finds the `statusLine` setting with
  Claude Code's precedence (managed > project-local > project > user settings; `CLAUDE_CONFIG_DIR` honored),
  starts its `command` in a shell, writes the session JSON to its stdin and shows its ANSI-coloured stdout
  (up to 4 lines, `padding` honored) under the input. The command is *run*, not parsed, so any language works
  through its shebang: a `#!/usr/bin/env python3` script runs under python, `#!/bin/bash` under bash, a `jq`
  one-liner or `node script.js` as written. Inline commands get bash or zsh (your login shell if it is one, else
  bash, never fish); `COLUMNS` and `LINES` carry the terminal size.
  - **Trust:** user and managed settings are used directly. A *project* or *local* status line runs only if
    Claude Code itself already trusts the folder (`hasTrustDialogAccepted` in `~/.claude.json`, checked up the
    parents); otherwise claude++ says so once and falls back, so opening a repo never runs its command.
  - **JSON** follows the documented schema: model, workspace (+ `repo` from `origin`, `git_worktree`), cost and
    durations, lines added/removed, context window (200000 unless the model reports more), `current_usage`,
    `exceeds_200k_tokens`, `fast_mode`, `effort`, `rate_limits` (windows past their reset are dropped),
    `session_name`, `version`, `output_style`, `transcript_path`, and `prompt_cache` (warm, ttl, expires_at,
    requests, misses by Claude Code's 5%/2,000-token rule, expected rebuilds after `/compact`, hit ratio...).
    Not provided: `vim`, `agent`, `pr`, `worktree`, `thinking`, `prompt_id`, `spend_limit`, and the miss causes
    other than an expired cache.
  - **When it runs** (the documented triggers): at start, when a reply arrives (end of turn), after
    `/compact`, on a permission-mode change, on `refreshInterval` (only while you have typed in the last 10
    minutes), when a limit window or the prompt cache reaches its reset/expiry time, plus model/effort changes
    and tab switches. Updates are 300 ms apart and a new one cancels the run in flight; a script gets 2 s.
    It steps aside while a permission prompt is up. Measured with a real 196-line Python script: ~27 ms and
    ~17 MB while it runs, then gone; 2 runs for startup plus one turn and none while idle.
  - claude++'s own line shrinks to what the script cannot know (mode, profile, cache hit/clock, memory), so
    nothing shows twice. `/statusline [on|off]` reports the command and its source and toggles it;
    `--statusline builtin` or `--statusline "<command>"` overrides the settings.
  - Differences: no clickable OSC 8 links (the text shows, unclickable); lines added/removed are counted from
    Edit/MultiEdit/Write calls (a Write counts all its lines as added).
- **Markdown**: headings, **bold**, *italic*, ~~strike~~, `code`, links (text then url), bullet / numbered / task
  lists with hanging indents, block quotes, rules, tables (aligned, shrunk to fit), and fenced code with a language
  label (diff blocks coloured). Unmatched markers stay literal so streaming text never flickers; `snake_case`
  and `2 * 3 * 4` are left alone. Hand-written, no new dependency; every line is checked to fit the width.
- **Inline images** (kitty / Ghostty): attached pictures and images in tool results (e.g. Claude `Read`ing a
  png) are drawn in the transcript. The picture is uploaded to the terminal once and the transcript holds
  plain text cells (kitty "unicode placeholders"), so scrolling and redraws are ordinary text and claude++
  keeps an id, not pixels. Other terminals, tmux, and replayed history show the text label instead.
  JPEG/GIF/WebP and PNGs over 1.5 MB are converted/downscaled with macOS `sips`; `CLAUDEPP_IMAGES=on|off` overrides detection.
- **`/command` completion**: type `/` for a full-width popup of claude++'s commands and Claude Code's (skills and plugins
  included, with argument hints). Tab completes, ↑/↓ choose, Enter completes a partial name and runs a complete one.
  The list comes from the `initialize` reply and is cached in `~/.cache/claude++/commands.json`, so completing
  does not start a backend; only the very first run, with no cache, starts one to learn the list.
- **Session list** (`/sessions`, `/resume`, ctrl+r, or ← with nothing left of the cursor, as in Claude Code): this
  directory's earlier conversations plus Claude Code's background sessions from every directory, read from
  `~/.claude/jobs/*/state.json` (no process spawned). Each row has its state (`needs you` and `working` first),
  age and title (your `/rename`, else Claude Code's generated title, else the first prompt); a preview of the end of
  the highlighted one with its summary or the question it is blocked on; and live filtering (all terms must match
  title, summary, directory or id; `s:work`, `s:needs`, `s:done` filter by state). Enter reopens it in this tab if
  it is blank, else in a new tab, working in that session's own directory; a conversation already open just switches
  to its tab. A session still running in the background cannot be resumed headless (Claude Code refuses), so Enter
  shows the `claude attach` / `claude stop` command instead. `/resume <id-prefix>` skips the browser.
  The list exists only while open: about 0.3 KB per row, 47 rows ≈ 14 KB, built in ~20 ms.
- **Fork, take over, stop** (in the list): ctrl+f opens a copy of the highlighted session in its own tab
  (`--fork-session`; works for any session, including ones open elsewhere). ctrl+t on a running background
  session stops it (`claude stop`) and opens it here; ctrl+x only stops it. Both need the same chord twice. Tool
  approvals granted "for the session" do not carry over.
- **Session config** (`/session`): a bottom panel for this tab only. Permission mode (same as shift+tab) and, per tool
  (Bash, Read, Edit, Write, Glob, Grep, WebFetch, WebSearch, NotebookEdit, Agent, plus any tool the session reports),
  `default` / `allow` / `deny`: ←→ steps through them, `+ add a rule…` takes any Claude Code rule such as
  `Bash(git push:*)` or `mcp__server__tool` (`d` removes one). Rules are sent as `permissions.allow/deny` through
  Claude Code's own settings channel, so they take effect on the running session at once (checked: a denied Bash
  call is refused with "Permission to use Bash has been denied"), add to your `settings.json` rules, and are
  re-applied if the tab's process is restarted by idle suspension. Not yet: saving them into a profile.
- **Side panel**: `/config` → Side panel → on (off by default; the full-screen list is always available and cannot be
  turned off). Then ← at the start of the input docks the list as a 30-column strip on the left (screens ≥ 100
  wide), focused: ↑↓ move, Enter opens, → closes it, Esc hands the keyboard back (typing does too) and leaves it
  docked, a second ← goes full screen, and → or Esc there step back to the dock. ctrl+r or `/sessions` hides and shows it. It re-reads its sources every 5 s while visible
  and draws only the rows around the cursor.
- **Scrolling**: mouse wheel (3 lines a tick), pgup/pgdn (a viewport minus two lines), shift+↑↓ (one line),
  ctrl+home / ctrl+end (top / latest). While scrolled up the lines in view stay put as output arrives, and a
  "↓ N lines below" tag shows how far back you are; sending a message returns to the latest. The wheel needs
  mouse reporting, so selecting text takes Shift (Option in some terminals); `/config` → Mouse wheel → off gives
  the mouse back to the terminal.
- **Custom profiles**: a JSON file per profile in `~/.config/claude++/profiles/<name>.json`, used like the built-in
  ones (`--profile name`, `/new name`, `/profile name`, or `"profile": "name"` in `config.json`; `/profile` lists them):

  ```json
  { "extends": "lean",                       // full | sources | lean | another profile; optional
    "append_system_prompt_file": "style.md", // adds to Claude Code's own prompt
    "system_prompt": "You are ...",          // or replaces it (also system_prompt_file); omit both to keep the default
    "tools": "Bash,Read,Edit", "setting_sources": "project,local", "strict_mcp": true,
    "mcp_config": ["mcp.json"], "model": "haiku", "permission_mode": "plan", "effort": "low",
    "add_dirs": ["../shared"], "args": ["--any-claude-flag"], "env": {"KEY": "value"} }
  ```

  A child's keys replace its parent's (`args` are appended, `env` merges, and either form of a prompt replaces
  both forms of the parent's). Relative paths are relative to the profile file; `~/` works. Unknown keys and
  `extends` loops are errors, not silently ignored. A new prompt only applies to a fresh conversation (Claude Code
  pins the prompt at the start); `/profile name --hard` re-renders it for the current one. Custom names are not
  in the `/config` cycle (it lists the built-ins); pick them with the commands above.
- **Usage** (`/usage`): where this tab's context goes and how much of your plan is used, straight from Claude Code
  (`get_context_usage` and `get_usage`, so no model call): per-category tokens biggest first (system prompt, tools,
  skills, memory files, messages, with on-demand and reserved parts dimmed), tool-result and hook-output sizes, and
  your 5-hour and weekly limits with time to reset. When skills are a big share of every request it says so and
  points at `/profile sources`. On this machine skills were ~10k of 17k tokens.
- **Optimize** (`/optimize [light|balanced|aggressive|off|now]`): trims what a *new* session loads, for this project,
  from evidence and without a model call. It reads this project's past transcripts (tools and skills actually used,
  plus the words the project's files and prompts use) and Claude Code's own skill list, proposes changes as a checkbox
  list with the tokens each saves, and saves the ticked ones to `<project>/.claude++/optimize.json` on Enter.
  `/optimize off` deletes it; no file of yours is touched. Levels (←→ in the panel): **light** caps skill descriptions
  (`skillListingMaxDescChars` 160); **balanced** also lists skills the project never used by name only
  (`skillOverrides: name-only`, `/name` still works) and shortens instruction sections unrelated to the project to their
  heading and first line plus a pointer to the file (the model reads the rest when it needs it; `[~]` in
  `/instructions`, `b` toggles it there); **aggressive** hides unused skills, shortens more, and narrows `--tools` to the
  core set plus what the project used (MCP servers left out when none was ever called). With under 3 past sessions it
  only caps descriptions: "never used" proves nothing yet. Checked on this machine from a directory under `~`: a fresh
  haiku session's first turn went 86.3k → 37.8k tokens of context (first-turn cost $0.015 → $0.005), with a question
  about a shortened section answered by the model going to the file. A conversation already under way keeps the prompt
  it started with (measured: applying the same setting live changed nothing), so `/optimize` applies at once to a tab
  that has not started or whose cache has expired, and otherwise says so and offers `/optimize now`, which re-renders
  the conversation once (re-caches its context). Past use is a heuristic: borderline cases get the gentle treatment
  (name-only, shortened) and every proposal shows its evidence.
- **Permissions** (`/permissions`): the saved `allow` / `ask` / `deny` rules from your user, project and local
  settings files, one tab per behavior with the file each rule comes from (and "overridden by deny" where a stronger
  behavior holds the same rule; deny beats ask beats allow). `+ add a new rule…` asks for the rule, then which file;
  `d` twice deletes a rule from its file; this tab's own `/session` rules are listed read-only. Only the list being
  changed is edited (other keys keep their order and values; a file that is not valid JSON is refused, never
  overwritten; a symlinked settings file is edited in place), and running sessions pick the change up by themselves:
  Claude Code watches its settings files (checked: a rule written while a session was open showed up in
  `get_settings` within 2 s). Not covered: the Workspace (additional directories) tab and managed policy rules.
- **Instructions** (`/instructions`): the `CLAUDE.md` / `AGENTS.md` files Claude Code loads for this directory (yours in
  `~/.claude`, then each folder up from here), as a checkbox tree with an estimated token size per file and section.
  Space unticks a file or one markdown section (a heading and what is under it); the files are never edited. Rules
  are saved in claude++'s config (`hidden`) and apply to tabs started afterwards: a hidden file goes into Claude Code's
  `claudeMdExcludes`, and a file with only some sections hidden is excluded and a filtered copy is appended to the system
  prompt (from `~/.cache/claude++/instructions/`; merged into the profile's own append prompt, since Claude Code takes
  one). No model call. Checked against the real thing: hiding a 150 KB `~/AGENTS.md` took a fresh haiku session from
  86.6k to 30.6k tokens of context; hiding one section removed exactly that section from the copy. A conversation that
  has started keeps the prompt it began with (Claude Code pins it), so a started tab says so instead of pretending.
  Sections are named by heading path, so a heading added later shows up ticked. The `lean` profile already skips
  these files (9.9k vs 86k prompt tokens on this machine).
- **Find** (`/find text`): jump to the newest entry containing the text (case-insensitive); `/find` again steps to
  older matches, `/find +` to newer, wrapping.
- **Undo** (ctrl+z): in the message box; a burst of typing is one step, other edits one each; forgotten on send and
  per tab. Closing a tab that is mid-turn asks for a second press.
- **Per-project profile**: `<project>/.claude++/config.json` with `{"profile": "lean"}` (any built-in or custom
  profile) sets the profile for tabs started there and for sessions opened from that project; `--profile` wins.
- **Small things that add up**: each tab keeps its own half-typed message when you switch away; `/copy` (or ctrl+y)
  copies the last answer and `/copy code` its last code block (OSC 52, plus `pbcopy` on macOS); alt+←/→ (or alt+b/f)
  move by word, alt+backspace and ctrl+w delete a word, alt+d deletes the next word, ctrl+k deletes to the end; a
  background tab that finishes a turn or asks for an answer rings the terminal bell or sends a desktop
  notification (`/config` → Background tabs: off / bell / desktop, OSC 9 terminals). SIGTERM, SIGHUP and crashes
  restore the terminal (mouse reporting, paste mode, keyboard mode) on the way out.
- A full-profile tab whose claude process passes 400 MB gets a one-time note saying what `lean` would save.
- Each tab has its own working directory (set when it opens a session from another directory); `@` files,
  the status line and `/resume` follow the tab in front.
- Resume with history: `-c`, `-r <id>`. Earlier messages are shown again, with tool calls and results.
  `/clear` starts a fresh conversation. `/model`, `/mode`, shift+tab cycles the permission mode.
- Any other `/command` (e.g. `/compact`, skills) is forwarded to Claude Code. A command whose
  only output is the turn result is shown as a note.
- Status line: model, context tokens in use, prompt-cache hit rate, session cost, rate-limit
  warnings, and RSS of the backends and the UI.
- Clean lifecycle: on quit every backend gets stdin EOF in parallel (so transcripts flush),
  and any that linger are killed. Verified: no orphaned `claude` processes after quit.

## Optimizations (measured, not assumed)

| What | How | Measured effect |
|---|---|---|
| Lazy backend | `claude` starts on a tab's first message | idle claude++ = **3 MB** RSS vs interactive `claude` idle at its prompt = **176 MB** |
| Warm on first keystroke | the backend starts when you begin typing (not at launch, not for `/commands`), so its ~0.6 s startup overlaps with composing the message; `initialize` makes no API call | memory stays 3 MB until you type; no startup wait after Enter (verified: no API activity before the first message) |
| Idle suspension | background tabs idle > `--idle-suspend` (120 s; a restart takes ~0.5 s) have their backend stopped; the next message respawns it with `--resume <id>` | frees ~220–330 MB per idle tab; context kept; **99% prompt-cache hit** on the first turn after resume |
| Memory budget | `--mem-budget <MB>`: when all backends together exceed it (measured as macOS `footprint` incl. child processes), stop the least-recently-used tab that is safe to stop; never the focused, busy, prompting tab, or one that started background work (heuristic: `run_in_background`, Monitor, Cron/ScheduleWakeup) | e2e with 2 real tabs and a 200 MB budget: LRU tab stopped (`1z`), focused tab kept, one `claude` left; warns (max once a minute) if only protected tabs remain |
| `--profile lean` | core tools + no MCP + `--setting-sources project,local` (skips user-level CLAUDE.md/AGENTS.md, skills, hooks) | prompt tokens/request in this repo: **85.8k → 8.9k**; cost of the bench turn $0.29 → $0.04 |
| `--sources` / `--tools` / `--no-mcp` | the same levers, individually | e.g. `--sources project,local` alone: 65k → 7k with the 4-tool set |
| `--env K=V` | passes env to every `claude` (Bun reads `BUN_JSC_*`; invalid names are rejected) | `BUN_JSC_gcMaxHeapSize=30000000` trimmed peak RSS ~11% (308M → 274M) on a light workload; not tested under heavy load. No hard cap: macOS doesn't enforce RLIMIT_AS/RSS |
| Cheap redraws | coalesced events + cached wrapping | 3,000-entry transcript, release build: 0.13 ms per redraw, 0.10 ms per streamed token (`cargo test --release -- --nocapture`) |

Run `claude++ --bench [prompt]` to reproduce the profile comparison on your machine.
It runs one prompt per profile and prints tools, init time, prompt tokens, output tokens,
cost, and peak RSS.

Caveats, stated plainly:
- Most of the default prompt is **cached**. Cache reads bill at ~10% of the input price, so
  the *cost* saving is smaller than the raw token saving. The raw saving still matters for
  context-window headroom and for cache writes on every cold start or 1-hour cache expiry.
- `lean` drops Task/subagents, Skills, MCP servers and other tools. That is a capability
  trade, so **`full` (no changes) is the default**.
- An **active** session costs about what Claude Code costs. The backend *is* Claude Code
  (≈220–330 MB). claude++ adds ~3 MB. The memory wins come from not running backends you
  are not using.
- Hooks and plugins the user has configured still run. For example, a memory plugin here
  spawns a separate ~300 MB node process per backend. That is the user's configuration,
  and claude++ does not override it.

## Profiles per tab, and the prompt cache

Each tab has its own profile (`full`, `sources`, `lean`): `--profile` sets the default, `/new lean` opens a
tab with one, `/profile lean` changes the current tab. Pick it when you start a conversation: Claude Code
**pins the system prompt and settings a conversation started with**, so a mid-conversation `/profile` only
changes tools/MCP (the conversation resumes with the new flags). `/profile lean --hard` re-renders the prompt
too, at the price of re-caching the conversation. Measured on haiku, resuming one conversation:

| Resume with... | cache read | cache write |
|---|---|---|
| same flags, 23 min idle | 10,192 | 186 |
| 3 tools added | 10,419 | 2,432 |
| tools removed | 12,892 | 155 |
| different `--system-prompt` (pinned: ignored) | 13,129 | 123 |
| different `--system-prompt` + `--system-prompt-snapshot off` | **0** | **9,475** |

What this means:
- The prompt cache survived 23 minutes idle and usage reports the 1-hour tier, so the status line counts
  down from 60 min (`cache 47m`, then `cache cold` plus a note). `--cache-ttl <min>` changes the assumed
  lifetime (use 5 if your account only gets the short cache). The countdown is an estimate from the last API
  response, not something the API reports.
- Staying with a tab inside the window is cheap; after it, the next message pays to re-cache the whole
  conversation. A `/new` tab does not.
- `/model` changes are warned about because caches are per model (documented API behavior; not measured here).
- **Compaction (reasoning, not measured):** `/compact` is itself a model call over the whole context. While the
  cache is warm that context is read at the cheap cached rate and the shrunk result is what gets cached
  afterwards. Once the cache is cold it pays the full write first. So if you will leave a long conversation
  for more than the window, compact before you go, not when you come back. claude++ never does this
  for you: an automatic extra model call would break the "no hidden LLM calls" rule.

## Benchmark (`bench/run.py`)

Controlled, per-process-tree measurements (haiku, one factor changed per config, shuffled runs, warm-ups
discarded). Results are in `bench/results/`. Summary of the first run: fresh-request prompt tokens
30.6k (stock) → 9.3k (`lean`) → 6.3k (`lean` + a short custom system prompt); real footprint of the `claude`
process 164 → 134 MB. All configs passed all 3 small tasks, but the tasks are too easy to show that a custom
system prompt does not hurt quality, so `lean` is the shipped profile and the custom prompt stays opt-in.
Use footprint, not RSS: RSS overstated memory about 2×.

## Limitations / not yet done

- Resume history is read from Claude Code's on-disk transcript (an internal format, parsed leniently and
  skipping what it does not recognise). Only the last 400 messages are replayed, to keep memory flat.
- Inline images need a kitty-graphics terminal with unicode placeholders (kitty, Ghostty). Not shown for
  replayed history. Terminals that report no cell size get a 1:2 cell-shape guess, so aspect can be slightly off.
- The list reads transcripts only for the current directory (background sessions from other directories come from
  the supervisor's state files, an internal format that is read leniently), searches titles, summaries and first
  prompts rather than full text, and cannot delete or rename.
- Markdown: no syntax highlighting inside code blocks, no nested block quotes inside lists, no HTML, no setext
  (underlined) headings, and a soft line break stays a line break rather than joining the paragraph.
- The `@` picker indexes up to 100k paths when it opens (cached for 30 s); a very large monorepo may feel it.
  Paths with spaces are not completed. The model picker shows every model Claude Code lists, including ones
  you may not have access to.
- No mouse scrolling (mouse capture would break native text selection). Use PgUp/PgDn or shift+↑/↓.
- Shift+Enter needs a terminal with the kitty keyboard protocol; alt+enter and ctrl+j always work.
- Tested on macOS only. Nothing is macOS-specific except that `ps` is used for RSS sampling.

## Tests

- `cargo test`: protocol state machine (streaming → final reconciliation, tools/results,
  permission queue, questions, interrupt, subagent nesting), wrapping / CJK width / cursor
  math, diff rendering, base64, and backend process lifecycle against a fake `claude` script
  (round trip, stderr capture, clean exit, kill on hang).
- End-to-end runs were driven through a pty plus a terminal emulator against the real backend:
  permission → file written, multi-turn recall, tabs, interrupt, AskUserQuestion, image
  attachment, idle suspend → resume with context, and quit with no orphans.
