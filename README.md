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

## Status: early, and what that means

claude++ is **unofficial**. It is not made, reviewed or endorsed by Anthropic; "Claude" and "Claude Code" are Anthropic's
names, used here only to say what it works with. It drives the official `claude` binary under your own login and adds no
model calls of its own. Licensed MIT (see `LICENSE`). Build from source (`./install.sh`, needs a Rust toolchain); there is
no prebuilt binary.

| | |
|---|---|
| **macOS, Apple Silicon** | the only platform it has been used on, day to day; every number in this file comes from it |
| **Linux** | builds and type-checks, and CI runs the tests on it, but nobody has used it by hand yet (a CI workflow is set up to run the tests there; it has not run yet, as there is no remote). Expect rough edges (memory figures fall back to `ps`, image paste needs `wl-paste`/`xclip`) |
| **Windows** | not supported (it needs a pty and POSIX process groups) |
| **Claude Code** | measured on 2.1.317 and 2.1.318. It leans on parts Claude Code does not document (flags such as `--autocompact`, `--system-prompt-snapshot`; control requests such as `get_context_usage` and `mcp_message`; the on-disk transcript and `~/.claude/jobs` state files). It says so when your `claude` is a newer line than it was verified on, and `python3 bench/contract.py` checks the ones that matter |
| **Terminals** | keyboard protocol, inline images, OSC 52 and colours behave differently per terminal; only the author's were tried |

Things to know before you rely on it:

- **Folders you open.** `claude -p` never shows Claude Code's "do you trust this folder?" dialog, and it starts a folder's
  `.mcp.json` servers and runs its project hooks without asking. So claude++ asks in its place: in a folder Claude Code has
  not recorded as trusted (`hasTrustDialogAccepted` in `~/.claude.json`) that has `.mcp.json` servers or project settings
  that run commands (`hooks`, `env`, `apiKeyHelper`, ...), it does **not** start `claude` and lists what would run. Open the
  folder once in `claude` and accept its dialog, or type `/trust` to start anyway for this run. A folder that brings nothing
  to run starts at once.
- **The benches spend real money.** `cargo test` and `bench/budget.py` are free; `contract`, `hostile`, `away`, `share`, `run`,
  `lanes`, `levers` and `tabs` call the real API: cents each, and the long sweeps (`levers`, `run`) up to a couple of dollars (one growth test cost about $2.30). The table at the end gives the cost per command.
- **Known flakes, not understood.** One `backend` unit test failed once while a release build ran alongside it (0 failures
  in 85 loaded reruns since); the `hostile` bench's permission scenario asks the model to run a command and
  retries up to three times if it declines (it also switches Claude Code's sandbox off for that run: with the sandbox on,
  a command outside the folder is blocked without a prompt, which made it fail three times in a row before this was found);
  one vim-editor bench run failed once and has not recurred.
- **Sharing MCP servers between tabs** (`/config`) is experimental and off by default; see its section for what it drops.

## Measured against `claude`

Claude Code 2.1.317, model haiku, macOS on Apple Silicon, 2026-11-03 (machine busy: load 6–14). Memory is macOS `footprint` summed over each
process tree (RSS overstates it about 2×); tokens are read from the session transcripts both frontends write.
Medians of 3 interleaved runs, each in a clean environment. Reproduce with `python3 bench/vs_claude.py 3` and
`python3 bench/tabs.py`; raw results are in `bench/results/`.

| | `claude` | claude++ |
|---|---|---|
| Open, nothing sent yet | 270 MB | **4 MB** |
| After one turn | 282 MB | **239 MB** |
| Three conversations left open, idle | 787 MB (3 windows, 15 processes) | **225 MB** (3 tabs, 6 processes) |
| Context tokens per request, same settings | 45.9k | **32.5k** |
| First paint | 0.56 s | **0.024 s** |
| `lean` (same flags for `claude`): tokens per request, memory after a turn | 9.8k, 179 MB | **8.7k, 120 MB** |

Where it comes from, and what it costs:
- **4 MB:** a tab starts its `claude` only when you begin typing a message, so its startup overlaps with typing.
- **225 vs 787 MB:** a background tab idle past `--idle-suspend` (120 s; 20 s in the bench) stops its `claude`, and
  the next message resumes the conversation. In the bench the resumed tab still knew its earlier answer, and its
  first request read 32.5k tokens from the prompt cache and wrote 117 (re-run on Claude Code 2.1.317, 2026-11-03).
- **32.5k vs 45.9k tokens:** claude++ runs Claude Code headless (`claude -p`), and on this version headless mode sends
  about 13.5k tokens of built-in tool definitions only when the model asks for one (ToolSearch), where interactive
  `claude` sends them with every request. This is Claude Code's behaviour, not a trick of claude++, and a version
  can change it. The first use of a deferred tool costs one extra round trip. Most of a request is cached, so the
  cost saving is smaller than the token saving.
- **Away from the keyboard:** the tab in front is stopped too after five times `--idle-suspend` (10 min) without input,
  when nothing is running (no turn, background task or command). Measured with `bench/away.py`: 238 MB right after a
  turn, **4.8 MB** once away (one process left), and the conversation was still there on the next message. Typing
  starts the backend again while you compose.
- **Staying true:** `python3 bench/contract.py` runs the real `claude` through everything claude++ relies on (control
  protocol, `init`, streaming, `--resume`, the unknown-id failure) and checks the 31k-token headless request above.
  Passing on Claude Code 2.1.317; claude++ prints a one-time note when it meets a newer minor version than the last
  one verified.
- **Not better:** keystroke echo (`claude` 0.3 ms, claude++ 1.6–6.7 ms: both imperceptible, claude++ slower) and turn
  time, which the API decides (0.9–4.7 s across all runs here; claude++ in `full` ran about a second slower on the
  medians, 3.9 s vs 2.9 s). The likely cause is by design: a message waits (at most 5 s) for MCP servers that are
  still connecting, so its first turn has their tools; the 8 servers on the bench machine settle 1.5 s after the
  process starts. The bench pastes its prompt at once and pays all of it; typing a message overlaps it. A tab you are using costs what Claude Code costs, because its backend *is* Claude Code.

## Architecture

```
┌──────── claude++ (one process, ~4 MB footprint) ──────────┐
│ main.rs     App, config at startup, the event loop        │
│ keys.rs     key routing (one front panel answers)         │
│ input.rs    message box: editing, completion, vim, paste  │
│ tabs.rs     tabs, sessions, the side list                 │
│ commands.rs `/` commands and sending  panels.rs  panels   │
│ cli.rs flags · tty.rs terminal · memory.rs link · sync.rs  │
│ limits.rs   plan-window warning and the hold on sends     │
│ supervisor.rs measuring, budget, idle · supui.rs panel    │
│ pane_host.rs the embedded editor (pane.rs: pty + vt100)   │
│ ui.rs       ratatui renderer (per-entry wrap cache)       │
│ session.rs  protocol → transcript state machine (per tab) │
│ backend.rs  child process + stream-json I/O               │
│ share.rs    one MCP server process for several tabs (opt-in)│
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
- **Rendering is O(visible lines).** Each transcript entry caches its wrapped lines per width, and only entries
  near the view keep them (the rest keep just their row count). Following the newest output, only the entries
  that fill the screen are wrapped at all, so a resize costs the same in a long session as in a short one.

Dependencies: `ratatui` (with its bundled crossterm), `serde_json`, `unicode-width`, `libc`, `signal-hook`, and `vt100` (the terminal emulator for the editor pane).
Base64 (for images) and argument parsing are a few lines each, so they are hand-written.
Release binary: ~2.3 MB.

## Habits from Claude Code that carry over

- ↑ history is Claude Code's own `~/.claude/history.jsonl` for this folder: prompts typed in either show up in both.
- The agent's task list (TaskCreate / TaskUpdate) stays in view above the input while any task is open.
- Keys: alt+p model picker, ctrl+z undo, ctrl+l repaint, shift+tab modes (bypass too when the
  backend was started with `-- --dangerously-skip-permissions`).
- `/cost` and `/status` open `/usage`; `/memory` edits `./CLAUDE.md` (`/memory user`: `~/.claude/CLAUDE.md`) in `$EDITOR`;
  `/export [file]` saves the conversation as text; `/mode` alone opens this tab's mode and rules.
- ctrl+r searches the messages you sent (this folder's, shared with Claude Code), as in Claude Code; the session list is ←.
- ctrl+b moves a running command to the background, as in Claude Code; `/tasks` lists background commands next to
  subagents, and `x` stops one. A tab with a command still running is never stopped by idle suspension or `--mem-budget`.
- The model's thinking shows as one folded `✻ Thinking…` line; ctrl+o reads it.
- Key hints are written the way your keyboard labels them, fixed when claude++ is built: `⌃t`, `⌥o`, `⇧⇥` on macOS
  (⌥ is Option, which terminals send as Alt), `Ctrl+T`, `Alt+O`, `Shift+Tab` on Linux.
- Differences: ctrl+t opens a tab; Claude Code commands that open an interactive panel
  (`/hooks`, `/login`, `/plugin`...) answer that they need the Claude Code terminal.

## What works (verified end-to-end against Claude Code 2.1.309)

- Streaming responses (token deltas) with a live activity line: requesting, thinking,
  calling a tool, elapsed time.
- Tool calls rendered Claude-Code style: `● Bash(cmd)`, `⎿` result preview, ctrl+o to expand everything, and alt+o to fold just the latest one. A command run shows its first lines, or its last when it failed; unfolded it shows the whole command and what it is for.
  `Edit` and `Write` show Claude Code's own hunks: "Added 2 lines, removed 1 line", numbered lines, context, and
  tinted bands for what changed (a pending edit shows a line diff of the call). `Read` shows only its size, as
  Claude Code does; an agent's report renders as markdown; answers to a question read `· question → answer`;
  `TodoWrite` is a checklist; other tools show their input as words, not JSON (MCP tools as `server · tool`).
- **Looks like Claude Code where it helps**: your messages as `❯` on a band, a `✻ Thinking… (12s · ↓ 340 tokens ·
  esc to interrupt)` line above the input while Claude works and `✻ Worked for 8s · done 14:30` after (the time it stopped), a
  one-row `❯` hint at the top of the transcript naming the message being answered whenever that message has scrolled off screen,
  syntax-coloured code blocks, a Bash permission prompt that shows the command and its description, and long choices that wrap.
  Colours work on a light or a dark terminal and follow it when it switches while claude++ runs; `/config` → Colors
  fixes them to dark or light instead.
- **`/tasks`**: this tab's subagents as a list (state `◐ working` / `✓ done` / `✗ failed`, type, what each was asked, step count);
  Enter opens one with its steps and report (a running agent updates live), esc backs out. Read from the transcript, so it costs no tokens.
- **Subagents**: each one's steps are listed under its own `Agent` call (parallel agents do not interleave), and
  its report becomes that call's result, whether it ran in the foreground or in the background (headless Claude
  Code runs agents in the background: the turn ends, the agent keeps working, and its report starts a new turn).
  A finished agent folds its steps into a count under its report; ctrl+o lists them.
- **Rate limits** (`/config` → the *Rate limits* tab). Claude Code reports your plan's 5-hour and 7-day windows; claude++ reads
  them from every tab, since they belong to the account. *Warn at* (70–95%, default 90%) says so once, when a window first
  reaches that: a note in the conversation, a message at the bottom and, with *Background tabs* set, a bell or desktop notice;
  it warns again only after that window resets. *Hold messages at* (90–100%, off by default) keeps a message you send out of
  Claude's hands while a window is that full: it waits in the queue (`alt+q`), due when the window resets, and messages that
  were already waiting for a turn join it. Slash commands still go at once, and a turn already running is not touched.
  *Send now* on a held message goes ahead anyway, and so does everything after it until that window resets. A window is
  only known once Claude Code has reported it (the status line's *limits* segment shows when it has), so a fresh tab
  holds nothing until its first turn. Saved as `limit_warn` and `limit_hold`.
- **Compacting before the cache runs out** (`/config` → "Compact before the cache expires", off by default): a tab that is idle
  with a conversation of at least the chosen size (100k to 400k) and under 30 s of prompt cache left gets `/compact` sent, while
  the cache can still serve the read at a tenth of the price. Your next message then re-caches a summary (plus the ~33k
  of system prompt) instead of the whole conversation at 1.25x (5-minute cache) or 2x (1-hour cache) the price. It costs
  working from a summary, and a compaction wasted if you were about to come back. A tab you are typing in, or one
  that is busy, asking, or stopped, is left alone. A claude++ that is not running cannot do it: nothing compacts while
  the terminal is closed.
- **Claude Code versions** (`/version`): lists the versions the native installer keeps in `~/.local/share/claude/versions` (marking the
  one your `claude` command runs, the newest, and the ones on the line claude++ was verified on), and `/version 2.1.317` pins claude++
  to one: new and restarted tabs run that file directly with Claude Code's own updater switched off for them (idle tabs switch on
  their next message; a busy one keeps its version until restarted). `/version default` unpins. Your `claude` command and symlink are
  not touched, nothing is downloaded (`claude install <x.y.z>` fetches a version), and `--claude-bin` wins over a pin. Saved as
  `claude_version`. An npm or Homebrew install keeps no such folder, so there is nothing to list or pin. `/statusline items` can show
  the running version as `cc 2.1.317`. Verified end to end against the installed 2.1.317 (the process on disk and the tab both report it).
- **`/cd <path>`** moves the tab to another folder (`~`, relative paths and `/cd -` work; `/cd` alone says where it is). The conversation
  goes along: from the next message Claude Code resumes it in the new folder, so tools run there and that folder's CLAUDE.md and
  settings load, while the transcript stays where it was. Refused mid-turn. Checked against the real thing: after `/cd` the
  next turn's `pwd` was the new folder and Claude still remembered what it was told in the first. Folders with their own
  commands still need `/trust`, as for any new folder.
- **`!command` runs in your login shell** (`$SHELL`: fish, nushell, zsh, bash, ksh, tcsh, elvish, xonsh, PowerShell), so it means what it
  means in your terminal; `/config` → "Shell for !" pins plain `sh` instead, and a shell claude++ does not know falls back to
  `sh`. The status line command stays on bash/zsh (those scripts are bash), and Claude's own Bash tool uses the shell Claude Code picks.
- **Opening links and files** (`/open`): a panel lists the `http(s)` links and the existing file paths of this conversation, newest
  first (what Claude said, the files it read or changed, what you wrote; `file.rs:42` works). Enter opens one with the system's
  opener (`open`, `xdg-open`), `e` edits a file in your editor, `/open 2` or `/open <link|path>` skips the panel.
  Only `http(s)` links are opened. Files that run code when opened (apps, scripts, installers, `.py`, `.js`...) are shown in the
  file manager instead of opened.
- **Worktrees** (`/worktree`, `/wt`): a panel of the repository's git worktrees with the tabs working in each. Enter opens one in a tab (the
  tab already there, else a new one), `c` moves this tab into it (like `/cd`), `n` adds one (a sibling folder `<repo>-<name>` on branch
  `<name>`, made or reused; the new tab opens when git is done), `d` twice removes one. `/worktree add <name> [base]` and
  `/worktree rm <name>` do it without the panel. Nothing is forced: git's refusal to remove a worktree with changes is shown as it
  is, the main worktree cannot be removed, and neither can one a tab is working in. Tested against real git, not a stand-in.
- **Swarm** (`/swarm [2-8] <goal>`): one message asking Claude to split the goal into independent parts and run each as a parallel subagent
  with its own git worktree (`isolation: "worktree"`). claude++ starts no extra processes: the subagents live inside the one `claude`
  process (about 4 MB each, against ~234 MB for a separate process), and `/tasks` shows them. Every agent bills its own tokens, so expect up
  to N times the usage; a note says so. Refused in the `lean` profile, which has no Agent tool. Checked against real Claude Code (haiku, 3
  files): one message with 3 parallel `Agent` calls, all `isolation: "worktree"`, 3 worktrees. Two honest limits: it is a prompt, so the model
  decides whether to follow it; and each agent's work stays on its own worktree branch until you merge it (`/worktree` lists them).
- **Side questions** (`/btw <question>`): ask something about the conversation while Claude is still working, without interrupting it. Nothing
  opens and no key is taken: the question appears in the transcript and the answer fills in below it (dim gutter, with its price), so you
  keep typing, answering prompts or watching the turn. One `claude -p` call resumes a copy of the conversation (`--fork-session
  --no-session-persistence`) with every tool off; the question and answer never reach Claude's history, context or transcript file. It
  goes out at once even with the message queue on, and several can be in flight. It re-reads the conversation as input (with no tools it
  may not hit the prompt cache; not measured), and it sees what Claude Code has already saved, so the step in progress may be missing.
  Not run against the real API here (the sandbox blocks it): the tests use a stand-in `claude` that checks the arguments and the question.
- **Terminals, for you and for Claude** (`/term [command]`): a real shell (or the command) on a pty inside claude++. It takes the whole
  screen and gets every key; ctrl+\ goes back to the conversation and the program keeps running. `/term` again lists them: Enter opens,
  `n` is a new shell, `x` twice closes one. With `/config` → *Agent terminal* on, Claude gets five tools hosted inside claude++ itself
  (the in-process MCP route the shared servers use, so no extra process): `term_open`, `term_send`, `term_screen`, `term_list`,
  `term_close`. That is what its Bash tool cannot do: a REPL, `vim`, `ssh`, a debugger, a dev server it keeps watching. `term_send`
  types text and named keys (`Enter`, `Ctrl-C`, `Up`, `F5`...) and returns the screen once the output goes quiet, or once the text
  in `until` appears; `lines` reaches back into 2,000 lines of history. You can watch Claude work in a terminal it opened and take over
  its keyboard. The reverse is closed: a terminal you open is private until you press `s` in the list, and every tool call goes through the
  usual permission prompt. Off by default, because the tool definitions cost roughly 650 tokens per request; nothing runs until a terminal
  is opened. Terminals belong to the tab, outlive restarts of its `claude` (`/cd`, `/profile`) and end with the tab; at most six per tab.
  Checked against real Claude Code (haiku, lean profile): it opened `python3 -q` and `sh`, ran code in both, read the output and closed them.
  Limits: the screen is what is read, not the raw stream (a program that redraws leaves only its last picture), the viewer has no
  scrollback of its own, and Claude's terminals are 120x40 until you look at one, which resizes it.
- **Panels**: every interactive surface (prompts, suggestions, model picker, config, session browser) is drawn by
  one shared component at the bottom of the screen above the input, so they look and behave alike; see
  [DESIGN.md](DESIGN.md). **`/config`** (or `/setting`) lists claude++'s own settings (status line, profile for new tabs, inline images,
  prompt-cache lifetime, idle-suspend time, memory budget...): ←→ changes a value, it applies at once and is saved
  to `~/.config/claude++/config.json`; a command-line flag still wins for that run. Claude Code's own settings
  are edited in its settings files (or `/config` in `claude` itself).
- Prompts the agent waits on, as one choice-list panel (see [DESIGN.md](DESIGN.md)): permission requests (↑↓ + Enter,
  or y / a / n at once; the "always" row says what it remembers, e.g. "auto-accept edits for this session" or
  "don't ask again for Bash(npm test:*)"; plan approval shows the plan as markdown and also offers auto-accept, as
  Claude Code does; on the "No" row typing says what to do instead, and Claude gets it with the refusal) and
  `AskUserQuestion` with tabs for several questions, a
  review step, checkboxes for multi-select, an inline "Other" answer (just start typing, or paste), and option
  previews. Esc declines. Pasted text can never act as a hotkey (on a permission prompt it becomes the reason for "No").
- Multi-turn conversations, Esc / ctrl+c interrupt (the process survives, and the next turn works).
- **Rewind** (Esc Esc on an empty input, as in Claude Code): pick an earlier message and restore the conversation,
  the files Claude edited since, or both; the message comes back in the input to edit and send. The conversation
  resumes at the entry before that message (`--resume-session-at`); files come from Claude Code's own checkpoints
  (`--rewind-files`, which claude++ turns on for its backends with `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING`).
  Edits made by shell commands are not checkpointed, as in Claude Code. Reopening a conversation shows the branch
  it continued on, not the turns a rewind abandoned.
- **Tabs**: independent sessions, each with its own backend. ctrl+t, ctrl+n / ctrl+p,
  alt+1..9, ctrl+d, `/new`, `/close`. A tab is titled with its conversation's name (`/rename`, else the title Claude
  Code generated) and wears its `/color` (red, blue, green, yellow, purple, orange, pink, cyan): the tab number or
  block, and, while the tab is in front, everything the UI draws in its accent (your messages, borders, the cursor,
  selections). Bare `/color` opens a picker that previews each colour on the whole UI, tab included, as you move; Enter applies it. Browsing the session list (side panel or full screen) likewise wears the highlighted conversation's colour. Both are Claude Code's own per-conversation settings, so they come back on resume.
- **Image attachments**: ctrl+v pastes an image from the clipboard (macOS via AppleScript, elsewhere `wl-paste` or
  `xclip`); `/img path`, or dragging a png/jpeg/gif/webp into the terminal, attaches a file. Each picture is
  **inline text**: an `[Image #N]` token on its own line at the cursor, with a small preview drawn right under it
  (kitty/Ghostty), so it sits among your words. The message is sent in that order, each picture introduced by its
  label so "compare Image #1 and #2" works, and the transcript shows the same layout at a larger size. Backspace
  or Delete next to a token removes it whole (and drops the picture); deleting the text is how you un-attach.
  History recall keeps the words only. Images over the API's 5 MB limit are re-encoded smaller first (macOS `sips`). A terminal that sends an *empty* paste for an image-only
  clipboard is handled the same way as ctrl+v (not verified in Ghostty; ctrl+v is the reliable key).
- **`!` shell commands**: `!git status` runs in the tab's folder right away, without a model turn, and shows like a Bash
  call. Its output (up to 30k characters in total, as in Claude Code, and 30k across all `!` commands waiting for your next message) is sent ahead of your next message in Claude Code's own
  `<bash-input>`/`<bash-stdout>` form, so you can ask about it. No timeout: a command that never ends never reports.
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
  usage credits). four menus, one with the keys (Tab moves it): models and versions as two lists side by side (↑↓), effort and fast mode as rows of choices (←→); Enter applies live (`set_model` and `apply_flag_settings`, no restart),
  and the choice is re-applied if the tab's backend is suspended and resumed. The list comes from `initialize`
  and is cached next to the command list; the status line shows `model effort`. `/model <name>` and `/effort <level>` set directly.
  A model switch warns that prompt caches are per model.
- **How working looks** (`/config`: Spinner, Progress bars, Motion; or edit `config.json`, which takes any characters you type).
  `"spinner"` is a sprite sheet: each character is one frame (`"⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"`), or space-separated words for multi-character frames
  (`"[=  ] [== ] [===]"`); `"none"` hides it. `"progress"` is two characters, filled then empty (`"█░"`); `"none"` keeps the numbers only.
  `"motion"` is `full` (spinner and a glint sweeping the verb), `calm` (default: spinner only) or `off` (nothing moves, and a busy tab
  redraws once a second instead of ~8 times a second). A value that does not parse falls back to the default. Bars appear only where there is a real
  fraction: usage, context, tasks done (in the spinner line) and agents done (`/tasks`).
- **Which status line is claude++'s own choice** (`/statusline claude | builtin`, remembered in
  `~/.config/claude++/config.json`; `--statusline claude|builtin|"<command>"` overrides without saving). `builtin`
  (the default) is claude++'s own single line and never spawns anything. `claude` runs your Claude Code status
  line: richer, at the price of a short-lived process per refresh (a Python script measured ~27 ms and ~17 MB each
  time). If `claude` is chosen but Claude Code has none configured, claude++ says so and shows its own line.
  The config file is a flat JSON object; unknown keys are kept when claude++ saves one.
  **`/statusline items`** edits the builtin line: a panel lists its parts (profile, model, context, cache hit, cache clock,
  cost, usage limits, memory, plus folder, git branch and clock, which are off by default); space shows or hides one,
  alt+↑↓ reorders, `r` resets, and the line under the message box changes as you go. The permission mode always comes first.
  Saved as `statusline_items` (a comma list in display order). The branch is read from `.git/HEAD`, no `git` process.
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
- **Session list** (`/sessions`, `/resume`, or ← with nothing left of the cursor, as in Claude Code): earlier
  conversations from every folder plus Claude Code's background sessions from every directory, read from
  `~/.claude/jobs/*/state.json` (no process spawned). Each row has its state (`needs you` and `working` first),
  age and title (your `/rename`, else Claude Code's generated title, else the first prompt); a preview of the end of
  the highlighted one with its summary or the question it is blocked on; and filtering (`/` starts it, in the side panel too; letters alone do nothing; esc leaves it; all terms must match
  title, summary, directory or id; `s:work`, `s:needs`, `s:done` filter by state). Enter reopens it in this tab if
  it is blank, else in a new tab, working in that session's own directory; a conversation already open just switches
  to its tab. A session still running in the background cannot be resumed headless (Claude Code refuses), so Enter
  shows the `claude attach` / `claude stop` command instead. `/resume <id-prefix>` skips the browser.
  The list exists only while open: about 0.3 KB per row, 47 rows ≈ 14 KB, built in ~20 ms.
- **Fork, take over, stop** (in the list): ctrl+f opens a copy of the highlighted session in its own tab
  (`--fork-session`; works for any session, including ones open elsewhere). ctrl+t on a running background
  session stops it (`claude stop`) and opens it here; ctrl+x only stops it. Both need the same chord twice. ctrl+x on an open tab that is working interrupts its turn at once (the side panel has it too). Tool
  approvals granted "for the session" do not carry over.
- **Delete** (in the list): Delete (or ctrl+k) twice removes the highlighted conversation's transcript from disk, for
  good. Conversations open in a tab or running in the background are refused.
- **Terminal title**: follows the active tab (`claude++ · name`) and is restored on exit (xterm title stack).
- **Session config** (`/session`): a bottom panel for this tab only. Permission mode (same as shift+tab) and, per tool
  (Bash, Read, Edit, Write, Glob, Grep, WebFetch, WebSearch, NotebookEdit, Agent, plus any tool the session reports),
  `default` / `allow` / `deny`: ←→ steps through them, `+ add a rule…` takes any Claude Code rule such as
  `Bash(git push:*)` or `mcp__server__tool` (`d` removes one). Rules are sent as `permissions.allow/deny` through
  Claude Code's own settings channel, so they take effect on the running session at once (checked: a denied Bash
  call is refused with "Permission to use Bash has been denied"), add to your `settings.json` rules, and are
  re-applied if the tab's process is restarted by idle suspension. Not yet: saving them into a profile.
- **Side panel**: `/config` → Side panel → on (off by default; the full-screen list is always available and cannot be
  turned off). Then ← at the start of the input docks the list as a 30-column strip on the left (screens ≥ 100
  wide), focused: ↑↓ move, Enter opens the highlighted conversation in a new tab (or this one if it is blank), → is "done browsing": it takes the highlighted conversation into the tab in front (replacing what that tab holds, or a new tab if it is mid-turn; switching if it is already open) and closes the side panel or leaves the full screen list, Esc hides it (the panel is either focused or gone; typing letters does nothing while it has the keys, `/` filters, ctrl+c keeps its usual job and does not leave it),
  a second ← goes full screen, and → or Esc there step back to the dock. `/sessions` hides and shows it. It re-reads its sources every 5 s while visible, on a thread (a list opens at once on what was read last, "loading…" the first time; titles are cached in `~/.cache/claude++/titles.json`, so only changed transcripts are read again; opening a conversation reads its transcript on a thread too, and sending waits until it is shown)
  and draws only the rows around the cursor.
- **Scrolling**: the mouse wheel scrolls the conversation (3 lines a tick; it never walks your input history), and so do pgup/pgdn
  (a viewport minus two lines), shift+↑↓ (one line), ctrl+home / ctrl+end (top / latest). While scrolled up the lines in view stay put as
  output arrives, and a "↓ N lines below" tag shows how far back you are; sending a message returns to the latest.
- **Selecting text**: claude++ takes the mouse, so it draws its own selection, live: the highlight follows the pointer as you drag, and
  what is selected is on the clipboard while you drag (OSC 52, plus `pbcopy` on macOS; at most ten times a second), with "copied N
  characters" shown when you let go. One mechanism for everything: the mouse and Shift are the same (claude++ asks the terminal to pass
  shift+drag on, xterm's XTSHIFTESCAPE, which xterm and Ghostty honour; a terminal that keeps Shift for its own selection still does),
  and the conversation, the message box and the panels all copy the same way. In the conversation the selection is held on the text,
  so it stays on the same words while an answer streams in or you scroll, and a pointer held above or below it keeps scrolling. A
  double click takes the word (paths and links count as words), a triple click the line; shift+click moves the end. Any key drops the
  highlight. **In the message box** a click puts the cursor there (vim's normal mode keeps it on a character), a drag selects, and
  shift+←/→/Home/End select from the keyboard (add ctrl or alt for words): typing or backspace replaces the selection. `/config` →
  Mouse → off gives the mouse back to the terminal for good (then no wheel and no clicks). `/copy` copies the last answer,
  `/copy code` its last code block.
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
- **Context** (`/context`): the context window drawn, from the same free `get_context_usage` reply `/usage` uses (no
  tokens spent). A gauge shows how full the window is, with the room held back for auto-compact at its end; under it one
  bar shows how the used part divides between the categories, both exact to an eighth of a character so small
  categories still show. Below, one row per category (largest first; tools loaded on demand, the auto-compact reserve
  and free space last). The row under the cursor decides the detail: skills by source and by size, memory files, MCP
  tools by server, agents, or what the conversation is made of (you, Claude, tool calls and results by tool,
  attachments such as hook output), plus the last request's cache hit rate. On a wide screen (90+ columns) the window
  is also a grid of squares to scale, the cursor row's squares drawn larger, and the detail sits beside the list;
  narrower, a shorter detail goes under it. ↑↓ move, `r` refreshes, Enter acts where claude++ has something for the
  row (skills: `/optimize`, memory files: `/instructions`), Esc closes.
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
  `/instructions`, `b` toggles it there); **aggressive** shortens more, replaces sections strongly unrelated to the
  project with a one-line pointer to their lines, and narrows `--tools` to the core set plus what the project used
  (MCP servers you defined are kept one by one: the ones the project called stay, the others are left out; all go when none was ever called; a project that called a plugin's or a connector's server keeps everything, since those cannot be listed in a config file). Nothing is hidden where the model cannot find it: a skill is at
  least listed by name (the Skill tool loads it when it turns out to matter), and every shortened or hidden section
  or file leaves a line saying where it is, so the model can read it. Checked with haiku, asking for a fact that only
  a hidden `CLAUDE.md` in the parent folder holds: with the pointer line, 3 of 3 runs answered right in 3 turns; with
  no trace (how a hidden file was handled before), 3 of 5 did, after 5–6 turns of searching. With under 3 past sessions it
  only caps descriptions: "never used" proves nothing yet. Measured with Claude Code's own count (2.1.312, 78 skills
  installed here): the skill listing goes 9.9k → 3.7k tokens with descriptions capped at 120 characters, and → 2.4k
  with unused skills also listed by name only. A conversation already under way keeps the prompt
  it started with (measured: applying the same setting live changed nothing), so `/optimize` applies at once to a tab
  that has not started or whose cache has expired, and otherwise says so and offers `/optimize now`, which re-renders
  the conversation once (re-caches its context). Past use is a heuristic: borderline cases get the gentle treatment
  (name-only, shortened) and every proposal shows its evidence.
  **`c` in the panel asks Claude** instead: one call to haiku (no tools, MCP, settings or saved session; the prompt
  is the project's folder listing, README start, tool and skill use, and every skill plus each instruction section
  over 1.5 kB with a 300-character preview) returns keep / shorten / hide with a reason per item, and those replace the
  word-overlap guesses at balanced and aggressive (balanced shortens what Claude would hide; light is unchanged).
  A skill the project has used is kept whatever Claude says. Measured here: 5 skills judged in 6 s for $0.0018.
- **Permissions** (`/permissions`): the saved `allow` / `ask` / `deny` rules from your user, project and local
  settings files, one tab per behavior with the file each rule comes from (and "overridden by deny" where a stronger
  behavior holds the same rule; deny beats ask beats allow). `+ add a new rule…` asks for the rule, then which file;
  `d` twice deletes a rule from its file; this tab's own `/session` rules are listed read-only. Only the list being
  changed is edited (other keys keep their order and values; a file that is not valid JSON is refused, never
  overwritten; a symlinked settings file is edited in place), and running sessions pick the change up by themselves:
  Claude Code watches its settings files (checked: a rule written while a session was open showed up in
  `get_settings` within 2 s). Not covered: the Workspace (additional directories) tab and managed policy rules.
- **Instructions** (`/instructions`): the `CLAUDE.md` files Claude Code loads for this directory (yours in
  `~/.claude`, then each folder up from here), as a checkbox tree with an estimated token size per file and section.
  Space unticks a file or one markdown section (a heading and what is under it), which leaves one line in its place
  naming the file and lines, so the model can still read it when it matters; the files are never edited. Rules
  are saved in claude++'s config (`hidden`) and apply to tabs started afterwards: a hidden file goes into Claude Code's
  `claudeMdExcludes`, and a file with only some sections hidden is excluded and a filtered copy is appended to the system
  prompt (from `~/.cache/claude++/instructions/`; merged into the profile's own append prompt, since Claude Code takes
  one). No model call. `AGENTS.md` is not listed: Claude Code 2.1.312 does not load it, beside a `CLAUDE.md` or
  without one (checked against its own list of loaded files). Measured with Claude Code's own count: hiding a 150 KB
  project `CLAUDE.md` takes a fresh session from 72.8k to 18.3k tokens, the pointer line left in its place costing 53;
  hiding one section removes exactly that section from the copy. A conversation that
  has started keeps the prompt it began with (Claude Code pins it), so a started tab says so instead of pretending.
  Sections are named by heading path, so a heading added later shows up ticked. The `lean` profile already skips
  your user-level `~/.claude/CLAUDE.md` (not the project's).
- **Find** (`/find text`): jump to the newest entry containing the text (case-insensitive); `/find` again steps to
  older matches, `/find +` to newer, wrapping.
- **Queue** (`/queue`, alt+q; `/config` → While Claude works: queue / send, to get the old send-at-once behaviour back): a message sent while Claude works waits instead of cutting in; a
  strip above the box lists the first few and each goes out when the turn ends. In the panel: ↑/↓ move, alt+↑/↓ (or
  J/K) reorder, `t` to the top, enter = send now (stops the running turn, then this goes first), `s` = steer (send
  without stopping; Claude takes it in at its next step), `e` edit (back into the box), `d` delete, `C` clear,
  `p` pause. Esc/ctrl+c to stop Claude holds the queue (the strip shows ⏸) so it does not start the next message
  behind your back; `p` or sending something new carries on. Slash commands are not queued. Pictures travel with
  their message.
- **Your editor, inside claude++**: `$VISUAL`, else `$EDITOR`, else `vi` (any shell command, e.g. `nvim -u NONE`; or
  `"editor": "..."` in config.json), running on a pty that claude++ draws with a terminal emulator (`vt100`), over a
  private temp file. Two uses, one component. **ctrl+g** opens it full screen on the message in the box: save and quit
  and the text replaces the box (ctrl+z undoes it); a failing editor leaves the message as it was. While it is open the
  conversation keeps updating behind it, and a hint row says if Claude is waiting for you. **Input mode `editor`**
  (`/config`, or `/editor`): the message area *is* your editor: it is open as soon as the mode is on (no `/` or key to start it), sized to a
  third of the screen above the status line, and a fresh one replaces it after each send. Claude's process still starts
  only when you type, as before. Quit the
  editor (`:wq`, `ctrl+x`...) and its text is sent as the message; quit with an error (`:cq`) and nothing is sent but
  your text goes into the box. **ctrl+\ always cancels** (nothing is sent), and is the only key the editor never sees.
  A permission prompt or question takes the keyboard from the pane until you answer it.
  claude++'s own keys still work: pgup/pgdn, alt+1..9, alt+o/q/p, shift+tab, ctrl+t/o/l always; alt+f/r/d/b/n and
  alt+←/→ stand for ctrl+f/r/d/b/n and ctrl+p/n (which the editor needs); and until you type something into the
  editor, ↑↓ walk your history (the message becomes the editor's text), ← opens the session list and Esc interrupts.
  ctrl+z is not passed on (it would suspend the editor).
  Memory: the editor is a process of its own and is open all the time in this mode, so a resting claude++ is its 4 MB
  plus the editor's: measured with the whole process tree, **vim 7.5 MB, nano 6.3 MB, nvim 21.4 MB** in total (2026-11-04).
  The 4 MB figure elsewhere in this file is for the other input modes. Not available in that mode:
  `/` and `@` completion, ↑ history, pasting images (a typed `/model` sent from the editor still runs). Tested with vim,
  nvim and nano (`bench/editors.py`); editors that need the kitty keyboard protocol or the mouse are not supported.
  If a pty cannot be opened, ctrl+g falls back to handing the terminal to the editor, and editor mode to the plain box.
- **Search** (ctrl+f, `/search [text]`, `/` in vim normal mode): filters this conversation as you type (messages,
  replies, tool calls, newest first) and scrolls to the highlighted match; enter stays there (and `/find` continues
  from it), esc goes back. Tab switches to the messages you sent before; enter puts one in the box.
- **Input mode** (`/config` → Input mode: default, vim or editor; remembered; `/vim` and `/editor` toggle). **Vim**: esc leaves insert mode. Normal: `h l j k` (rows as drawn) `0 ^ $ w b e W B E gg G`
  `f F t T ; ,`, counts, `x X D C s S r J ~ p P u ctrl+r`, `i a I A o O`, operators `d c y` with motions, `dd cc yy`
  and `iw aw i" a" i( a( i[ i{ i<` objects, `v`/`V` visual mode. enter still sends (and returns to insert), esc in
  normal mode still interrupts. Not there: `.` repeat, macros, named registers, `:` commands.
- **Scheduled messages**: `/schedule` lists times (in 5 minutes ... 4 hours, next 9:00 / 18:00) with the cursor on one; ↑↓ and Enter pick, or just type
  your own (`45m`, `2h30m`, `14:30`, 24-hour local). Then type the message in the ordinary message box: the box says it is scheduled, Enter adds it, Esc
  cancels. `/schedule 2h` skips the list and `/schedule 10m check the build` does it all at once. A scheduled message waits in the same list as queued
  ones (alt+q: reorder, edit, delete; Enter sends it now; `S` gives a waiting message a time, or takes it off with the first row). It goes out
  when the time comes and the tab is free; ordinary messages do not wait behind it and stopping Claude does not pause it. It lives in this
  claude++ process: quit and it is gone.
- **Undo** (ctrl+z): in the message box; a burst of typing is one step, other edits one each; forgotten on send and
  per tab. Closing a tab that is mid-turn asks for a second press.
- **Per-project profile**: `<project>/.claude++/config.json` with `{"profile": "lean"}` (any built-in or custom
  profile) sets the profile for tabs started there and for sessions opened from that project; `--profile` wins.
- **Small things that add up**: each tab keeps its own half-typed message when you switch away; `/copy`
  copies the last answer and `/copy code` its last code block (OSC 52, plus `pbcopy` on macOS); alt+←/→ (or alt+b/f)
  move by word, alt+backspace and ctrl+w delete a word, alt+d deletes the next word, ctrl+u / ctrl+k cut to the start / end of the line (at its edge, the line break); ↑/↓ move between the rows of a longer message and only then walk history (the half-typed message comes back when you step past the newest); ctrl+z undoes and ctrl+shift+z (alt+z where the terminal cannot tell shift) redoes; a
  background tab that finishes a turn or asks for an answer rings the terminal bell or sends a desktop
  notification (`/config` → Background tabs: off / bell / desktop, OSC 9 terminals). SIGTERM, SIGHUP and crashes
  restore the terminal (mouse reporting, paste mode, keyboard mode) on the way out.
- A full-profile tab whose claude process passes 400 MB gets a one-time note saying what `lean` would save.
- Each tab has its own working directory (set when it opens a session from another directory); `@` files,
  the status line and `/resume` follow the tab in front.
- Resume with history: `-c`, `-r <id>`, `/resume <id-prefix or name>` (any folder). Earlier messages are shown again, with tool calls and results.
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
| Lazy backend | `claude` starts on a tab's first message | open with nothing sent: **4 MB** footprint vs **268 MB** for `claude` at its prompt (see the table at the top) |
| Warm on first keystroke | the backend starts when you begin typing (not at launch, not for `/commands`), so its ~0.6 s startup overlaps with composing the message; `initialize` makes no API call | memory stays 4 MB until you type; no startup wait after Enter (verified: no API activity before the first message). A message sent before the MCP servers finish connecting waits for them (at most 5 s, "connecting MCP servers"), so the first request is not sent without their tools: an instant first message measured 32.5k context tokens, the same as a later one (31.0k, MCP tools missing, before this) |
| Idle suspension | background tabs idle > `--idle-suspend` (120 s; a restart takes ~0.5 s) have their backend stopped; the next message respawns it with `--resume <id>` | three idle conversations: **225 MB** vs 787 MB in three `claude` windows; context kept; the first request after resume read 32.5k tokens from cache and wrote 117 (99.6%) |
| Memory budget | `--mem-budget <MB>`: when all backends together exceed it (measured as macOS `footprint` incl. child processes), stop the least-recently-used tab that is safe to stop; never the focused, busy, prompting tab, or one that started background work (heuristic: `run_in_background`, Monitor, Cron/ScheduleWakeup) | e2e with 2 real tabs and a 200 MB budget: LRU tab stopped (`1z`), focused tab kept, one `claude` left; warns (max once a minute) if only protected tabs remain |
| `--profile lean` | core tools + no MCP + `--setting-sources project,local` (skips user-level CLAUDE.md, skills, hooks) | a fresh request: **30.4k → 9.3k** tokens; three small coding tasks cost **$0.0061–0.0075 → $0.0017–0.0019** each, all passed; the backend's process tree **243 → 132 MB** (`bench/run.py`, 2026-10-28, below). Large user-level instruction files (`~/.claude/CLAUDE.md`) count on top and are skipped too. Plus `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` (telemetry, autoupdate, `/ultrareview`, `/advisor`, `/import`, design): footprint **134 → 123 MB**, prompt **9,407 → 8,741** tokens (3 interleaved runs each, identical within a config) |
| `-- --setting-sources …` / `-- --tools …` / `-- --strict-mcp-config` | the same levers, individually (anything after `--` goes to claude) | a fresh request, 30.4k tokens by default: `--setting-sources project,local` alone 19.5k, the lean `--tools` set alone 11.6k |
| `--env K=V` | passes env to every `claude` (Bun reads `BUN_JSC_*`; invalid names are rejected) | `BUN_JSC_gcMaxHeapSize=30000000` trimmed peak RSS ~11% (308M → 274M) on a light workload; not tested under heavy load. No hard cap: macOS doesn't enforce RLIMIT_AS/RSS |
| Cheap redraws | coalesced events (at most ~30 frames a second while streaming) + cached wrapping, kept only near the view | 3,000-entry transcript: 0.2–0.5 ms per redraw (release build on a busy machine, `cargo test --release redraw_cost -- --nocapture`). 6,000 entries (3,000 command outputs, 3,000 markdown replies): 94 MB held, 86 MB of it the conversation itself (304 MB before rendered lines were dropped away from the view); a resize redraws in 5 ms while following the newest output (2.4 s before), 0.5 s when scrolled up (`cargo test --release zz_long_session_memory -- --ignored --nocapture`) |

Reproduce the profile comparison on your machine with `python3 bench/run.py` (see Benchmark below).

Caveats, stated plainly:
- Most of the default prompt is **cached**. Cache reads bill at ~10% of the input price, so
  the *cost* saving is smaller than the raw token saving. The raw saving still matters for
  context-window headroom and for cache writes on every cold start or 1-hour cache expiry.
- `lean` drops Task/subagents, Skills, MCP servers and other tools. That is a capability
  trade, so **`full` (no changes) is the default**. For subagents on a lean budget see
  [Many agents, one process](#many-agents-one-process).
- An **active** session costs about what Claude Code costs. The backend *is* Claude Code
  (about 115–255 MB after a turn here, depending on profile). claude++ adds ~4 MB. The memory wins come from not running backends you
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

## More levers, as profile `env` (measured; each trades a capability)

None of these are on by default. Put them in a profile file's `"env"` (see PROFILES). Prompt tokens are the first
request of a trivial turn on haiku, deterministic unless noted; "lean" means the lean profile above.

| Variable / flag | Effect | What you lose |
|---|---|---|
| `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` | −695 tokens (lean) | auto-memory prompt and writes |
| `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1` | −462 tokens | git/commit guidance in the Bash tool text |
| `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` | −205 to −413 tokens | background Bash |
| `"tools": "Bash,Read,Edit"` | −1,220 tokens (lean) | Write, WebFetch, WebSearch, AskUserQuestion |
| `--append-system-prompt "Be concise: answer in at most 3 short sentences, no preamble."` | about −30% output tokens on explanation prompts, none on tool-heavy work (1 task per type, haiku) | detail; a softer "Be concise." gave only −8% |
| `BUN_JSC_gcMaxHeapSize=64000000` (16–64 MB) | −19 to −29 MB; no effect at 128 MB+; does not stack with the traffic variable | a 25-turn session (context past 100k tokens, `bench/levers.py grow`): **no effect**, 159 MB with and without; it only trims the first minutes |
| `CLAUDE_CODE_DISABLE_WORKFLOWS=1` | −2.0k tokens on `full` (the Workflow tool; `lean` has no such tool) | the Workflow tool |
| `--disable-slash-commands` (as `args`) | −4.8k tokens on `full` (the skill list); nothing on `lean` | every skill and custom command |
| `CLAUDE_CODE_DISABLE_ATTACHMENTS=1` | **−11k tokens on `full`** | the model stops seeing the skill list, deferred tools (WebFetch, SendMessage, …), MCP tools and their instructions |

Measured and not worth it: `--agents '{}'` on lean (no change), `CLAUDE_CODE_DISABLE_BUNDLED_SKILLS` (**+4.6k** tokens on `full`: the skills come back in a longer form), `--no-session-persistence`,
`BUN_JSC_forceRAMSize` / `useConcurrentGC` / `maxPerThreadStackUsage` (no change), `BUN_JSC_useJIT=0` (**crashes claude**),
`MAX_THINKING_TOKENS` / `--effort` on haiku (no output change; thinking models untested). `--bare` and
`CLAUDE_CODE_SIMPLE=1` ignore OAuth and need `ANTHROPIC_API_KEY`. `full`-profile numbers swing by thousands of tokens
whenever MCP servers or plugins load. The only RAM lever above ~20 MB is leaving user-level MCP/plugins/hooks out
(`sources`/`lean`: about 237 → 135 MB).

**Swept, found nothing** (`python3 bench/levers.py envs`, `full`, `think`, `hidden`, `grow`; Claude Code 2.1.318, 2026-11-03).
Every boolean variable the binary reads that could plausibly matter (about 50: telemetry, checkpointing, cron, memory sync,
file watchers, growthbook, agent summaries, thinking switches, ...) was set one at a time on `lean`: beyond the four in
the table above, **none changes the prompt by more than 5 tokens, the footprint (it jumps between 126 and 138 MB from
run to run with no variable behind it) or the idle CPU (about 1% of one core, 0.1 s per 10 s)**. Other results:
a Bash turn on a larger model shows **no side calls to another model** in the result's `modelUsage` (titles, command
checks and agent summaries do not run under `-p`), so nothing hides in the bill; thinking and effort switches (`--effort`,
`MAX_THINKING_TOKENS`, `CLAUDE_CODE_DISABLE_THINKING`) moved output between 179 and 644 tokens on one reasoning prompt
with no order to it, so they are noise at this size and untested on long tasks; the backend grows about 1.3 MB per turn
(126 → 159 MB over 25 tool turns); on `full`, `--strict-mcp-config` alone is worth **−90 MB** (the servers are
child processes) and `--setting-sources project,local` alone −14k tokens and −58 MB. That run cost about $2.30 in
API fees: a 25-turn session re-reads its whole growing context every turn.

**The biggest lever is how long a conversation may grow** (`python3 bench/context_tax.py`, which reads only the token
counts in your own transcripts). Every request re-reads the whole conversation from the prompt cache, so in a long
session that read is most of the bill. On the author's 26k requests in 374 conversations: cache reads were **68% of
the spend** (writes 19%, output 13%, at the usual price ratios), **85% of those reads happened at a context over 200k**
and a third over 400k. Claude Code compacts on its own only near the model's full window (**967k** tokens by default, on
every model; `--autocompact` accepts 100k to 1M). Rough estimate for those transcripts if it had compacted at 200k /
300k / 400k instead: **38% / 24% / 14%** less spend, the compactions' own cost included (about 127 / 65 / 30 of them).
The price is working from a summary sooner, which no benchmark here can price for you, so it is a setting and not a
default: `/config` → "Compact the conversation at" (`autocompact_k` in `config.json`: 150, 200, 300, 400 or 600; 0 is
Claude Code's own), passed as `--autocompact <n>k` to conversations started or resumed from then on. A conversation past
400k tokens gets a one-time note saying what each turn now re-reads and how to change it (none once a size is chosen).
The same script shows the other cost, the cache writes: after a pause of 5 to 60 minutes only 3.8% of the author's requests
wrote their context again (the 1-hour cache is on), after more than an hour 74% did, about 55M tokens, a third of all writes.
Nothing but a hidden keep-alive call would stop that, which claude++ will not make; compacting before you leave makes the
re-write smaller.

**MCP servers cost very different amounts** (`python3 bench/levers.py mcp`, the process tree of one backend on `full`, each server alone against none):

| server (stdio, defined by you) | memory | prompt tokens |
|---|---|---|
| `entropy` | +13 MB | +963 |
| `caveman` | +24 MB | +137 |
| `comfy-mcp` | +73 MB | +2,354 |
| `codegraph` (spawns node helpers) | +75 MB | +603 |
| `blender` (python + uv) | +92 MB | +767 |
| all connected at once (plus a failed plugin one and four claude.ai connectors, which are remote and cost no process) | +98 MB, +2.8k tokens | |

Hence `/optimize` aggressive keeps the servers a project called and leaves the others out (`mcp_keep` in
`.claude++/optimize.json`), instead of all or none; a new tab in a folder that never used `blender` no longer starts it.

**Sharing MCP servers between tabs** (`/config` → Share MCP servers, off by default; `python3 bench/share.py`). Every tab's
`claude` starts its own copy of every MCP server you defined, so three tabs in one project run three codegraphs (four
processes each). With sharing on, claude++ starts each stdio server once and gives the tabs a hosted one instead: the
SDK mechanism (`initialize.sdkMcpServers`, the backend sends `mcp_message` control requests over the pipe it already
has, claude++ answers from one child process). The model sees the same `mcp__<name>__<tool>` tools. Three tabs asked,
in the same folder, to call three servers (synthetic ones with 40 MB each, next to your own caveman and codegraph):

| 3 tabs, 3 + 2 servers | processes | memory | server processes started | tool calls answered by |
|---|---|---|---|---|
| each tab its own copy (off) | 25 | 1,168 MB | 9 + the user's | 9 different processes |
| shared (on) | **11** | **697 MB** | 3 + the user's, once | **3** processes, all 9 calls |

(`bench/results/*-share.json`, 2026-11-04.) The process stops with the last tab that uses it and is restarted, and shaken
hands with again, if it dies. Nothing is left after a quit or a `SIGKILL` of claude++. What it costs, which is why it is off:

- **Plugin and claude.ai connector MCP servers are not loaded** in this mode: sharing needs `--strict-mcp-config` so a tab
  does not start its own copy as well, and those servers cannot be put in a config file. Remote (http/sse) servers you
  defined stay with `claude`. A tab says so once, when it starts. The profile `lean` (no MCP) and `/optimize` strict leave
  the tab alone.
- Only tabs with the same command, arguments, environment and **folder** share a process.
- A shared server cannot send progress, `list_changed` or logging notices to a tab, and a request it makes of its client
  (sampling, roots, elicitation) is refused. A server that keeps state per conversation should not be shared.
- A server that exits mid-call fails that call; the next one starts it again (three starts in a row without a finished
  handshake and it is given up on).

One `claude` process cannot host several conversations (its protocol binds a process to one session; `claim_session` only
binds a pre-warmed spare), so this, subagents (above) and `/optimize` are what is left to reduce.

**Tool output caps, measured and left alone** (`BASH_MAX_OUTPUT_LENGTH`, `CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS`; haiku,
an exact command so the runs are comparable). A result under the cap goes into the conversation whole and is re-read by
every later request: `seq 1 4000` (19 KB) added **9.8k tokens** to the next request (28.7k vs 18.9k). A result over the
cap, or any result over ~30k characters whatever the setting, is saved to a file and the model gets only the *head*
(about 2 KB, here ending at 527) with the path, and is told so: the same turn costs the baseline 18.9k, and
`BASH_MAX_OUTPUT_LENGTH=8000` or `2000` turned the 19 KB result into that preview (7 KB at 2000, not at 8000). The head is the
wrong part for a build or test log, whose failure is at the end, and the model has to read the file to see more (one more
turn). So the cap is a trade, not a saving: it is worth it for workflows that dump data and not for ones that run tests.
Not on by default; put it in a profile's `env` if your sessions are like the first kind.

**On by default: `--exclude-dynamic-system-prompt-sections`.** It moves the working directory, OS and git status from
the system prompt into the first message; the model still reports all three, and `--resume` still works. Two new
sessions in different folders then share the whole system prompt in the prompt cache: a new session's cache *writes* fall from
3.2k to 1.8k tokens (−45%; cost of a trivial haiku turn $0.0007 → $0.0004, `python3 bench/levers.py xcache`),
and a repository whose git status changed no longer invalidates it. `"stable_prompt": "off"` in `config.json` turns it off.

**A long conversation does not make the backend big, and `/compact` does not shrink it** (`python3 bench/levers.py growth 16`, Claude Code
2.1.321, haiku, `lean` backend, $0.26): over 16 turns the context grew from 8.8k to 37.8k tokens and the footprint from 139 to 147 MB (about
0.3 MB per 1k tokens); `/compact` cut the context to 12.3k tokens and the footprint stayed at 148–150 MB. So the ~140 MB a backend holds
is almost all baseline, and stopping the process (idle suspension, the budget) is the only thing that gives it back. Not measured: whether
the 0.3 MB per 1k tokens stays linear at several hundred thousand tokens.

The Claude daemon and `bg-spare` workers seen in `ps` come from interactive `claude` (`←`, `/bg`, `claude agents`), not
from `claude -p`; a headless run does not start or need them (checked by diffing `ps` around one run).

## Seeing and managing processes: `/procs` and the supervisor

`/procs` (or `claude++ procs` from a shell) lists every Claude-related process on the machine, grouped by who owns it, with
the memory each holds (macOS `footprint`; RSS elsewhere, which overstates). It is read-only. **Only what claude++ itself started is ever
controlled; everything else is shown and left alone**: interactive `claude` windows, Claude Code's own daemon and its
pre-warmed `bg-spare` workers, and MCP servers started by Claude or by another app. Groups: ours, the supervisor, another claude++,
`claude` (not ours), daemon, daemon spare workers, background session hosts, MCP servers (not ours). One run on a machine with
three `claude` sessions (two interactive, one background), Claude Code's daemon and one claude++ showed 44 processes holding 2.6 GB, of which the daemon's three idle
spare workers held 0.68 GB: that is Claude Code's, so it is listed and not touched (`CLAUDE_CODE_DISABLE_AGENT_VIEW=1` in the
shell that starts `claude` is Claude Code's own switch for its daemon; not tested here).

Once a day at most, claude++ also says so in the first tab when Claude Code's idle spare workers hold more than 200 MB, so the cost of
interactive `claude` windows is visible without opening `/procs`.

**The supervisor** (`claude++ supervisor`) is one small process per user that every claude++ connects to. claude++ starts it when none
is running, and it ends 20 s after the last claude++ has left and any clean-up below is done. **It owns every decision about memory and
idleness, and does the measuring**; a window only says what its tabs are and obeys the request to stop one (it can refuse a tab you
just switched to or that started working). It has these jobs:

- *Status:* each claude++ tells it what its tabs are doing, and it tells every instance about the others (`/procs` shows the other
  windows' tab counts, busy tabs and memory).
- *A ledger of what claude++ started:* each instance reports the pids of its `claude` backends. If an instance disappears without a
  goodbye (killed, crashed), its backends that are still running are stopped, 10 s later with SIGTERM and after 3 s more with SIGKILL.
  Guards: the instance itself must be gone for good (same pid *and* start time no longer running, so a window that only lost its
  connection keeps its processes), and each pid must still have the start time it was reported with (a reused pid is left alone).
  A clean quit says goodbye first, and then nothing is touched.
- *Measuring.* It runs `footprint` (or `ps`) for all windows at once, every 6 s while any window set a budget and every 18 s otherwise:
  each tab's `claude` process with its children, and each window itself. The numbers come back to every window with the peers message,
  and are what the `mem` segment of the status line, `/procs` and the hints show. A window runs no `ps` or `footprint` of its own.
- *One memory budget and one idle limit for all windows.* `--mem-budget` and `--idle-suspend` (or `/config`) are settings of the
  supervisor, not of a window: one value for everyone, saved in the config file, and a flag sets it for as long as the supervisor
  runs. When all windows together hold more than the budget, the supervisor asks the window that owns
  the longest-idle tab that is safe to stop (same rules as before: never the tab in front, a busy one, one with a prompt or background
  work) to stop its `claude`; it resumes on the next message, which re-caches the conversation. One tab per check (every 6 s), so the
  next memory sample shows the effect before another goes. The owning window has the last word and refuses a tab you have just switched to or that started working. When the total is over
  the budget and no tab is safe to stop, the supervisor tells every window (at most once a minute).
- *Idle tabs, and the window you are in versus the ones you left.* A tab that is not in front is asked to stop once it has been idle for
  the idle limit (`--idle-suspend`, default 120 s). The tab in front of the window you are working in is asked to stop after five times as long with no
  input. The tab in front of a window you left is treated like a background tab. Each window tells the supervisor
  whether its terminal has focus (terminals that report it) and how long since its last key. The supervisor
  decides which window is in front: the one whose terminal has focus, else the one with the most recent input (and any other within a
  minute of it). Every other window's front tab is then as stoppable as a background tab: it is asked to stop once idle for the idle
  limit, **with or without a memory budget**, and the budget picks from those tabs too. The owning window refuses when its
  terminal has focus (unless you have been away five times the idle limit) or you typed in the last 30 s. A window that reports nothing (an older build) counts as in front. Measured with `bench/multi.py`
  (`sep-active`: three separate windows, typing only in the third, haiku): 453 MB with all three running, **167 MB** after two minutes,
  against 447 → 434 MB with the supervisor off; the two stopped windows answered their next message 1.6–2 s later than the one in use.
  A window you left is stopped 60–90 s after you start working elsewhere at the earliest (the 60 s margin plus the 30 s reporting step),
  and not before its tab has been idle for `--idle-suspend`. The idle sweep is part of the supervisor's budget switch: when you pause the budget (the "paused" switch in the supervisor panel), nothing is stopped.
  `CLAUDEPP_SUP_DIR` points a claude++ at a different supervisor directory (benches and tests); that directory is created with mode 0700.
  A supervisor left running from an older build (a rebuild or an upgrade) is replaced by the first window of the new build that finds it serving nobody else; one with other windows attached is left alone, and the panel keeps saying "stale" until they close.
- *Shared MCP servers across windows.* With *Share MCP servers* on (`/config`), the stdio servers of every tab are run once instead of
  once per tab. Without the supervisor that is one process per window; with it, one process for **all** windows (same command,
  arguments, environment and folder). A window hands each JSON-RPC message to the supervisor and gets the answer back; request ids are
  kept apart per window. The process stops when the last window that used it is gone. If the supervisor goes away, a message in flight
  gets a JSON-RPC error and the next ones are answered by a process of the window's own, as before. Off by default, like the setting.
- *Nothing else.* No process claude++ did not start is ever signalled.

**Without the shared supervisor.** A window that cannot use it (`CLAUDEPP_SUPERVISOR=off`, stopped with `/supervisor`, or one that would
not start) runs the same supervisor code inside itself, over a socket pair, for itself only. Budget, idle-suspend and measuring work the
same, with this window's tabs only, and its tabs share MCP servers among themselves when that setting is on. There is no second
implementation of any of it in the window. A window on its own supervisor looks for the shared one again every 10 s (unless `off`) and
switches to it when it is there.

**Managing it: `/supervisor`**, a dashboard. From the top: its state (running, pid, uptime, protocol; a warning when it was started from an
older claude++ binary), the figures at a glance (windows, tabs and how many are working or hold a claude process, shared MCP servers;
since it started: tabs the budget stopped, processes cleaned up after crashed windows, MCP messages relayed, windows served),
**memory** as a gauge against the smallest budget any window asked for plus a sparkline of the last two minutes, every **window** with
its share of memory and each of its tabs (working, idle for how long, or no process), the **shared MCP servers** it runs (pid, memory,
windows using it, times started), and its **activity** (the log, with times). It refreshes every second. The same screen is the
control panel: ↑↓ moves over the MCP servers, the settings and the buttons, ←→ changes a setting or picks a button, Enter runs.
*Settings*, each saved in `config.json` and applied to the running one at once: how long it **stays after the last window** (20 s, 5 min,
1 hour, always), whether it **cleans up after a crashed window** (on by default), and whether the **memory budget** is enforced or
paused. *Buttons*: **Restart** (windows reconnect within seconds and a new one starts from the current binary: what to do after
updating claude++), **Stop** (and keep it stopped until you Start it; shared MCP servers then run in each window), **Start**; Enter on a
shared MCP server restarts that process (the next message from any window starts it again). A supervisor from before this panel cannot
be inspected, so the dashboard shows only what the windows already know of each other, and Restart and Stop signal it, only after its
command line says it is a `claude++ supervisor`. It never acts on anything claude++ did not start.

The socket is `/tmp/claudepp-<uid>/sup.sock` (directory 0700, socket 0600, the directory's owner is checked); the log next to it is
`supervisor.log`. Messages are one JSON object per line (`hello`, `tabs`, `mcp`, `bye`, `status`, `set`, `mcp_restart`, `shutdown`; `welcome`, `peers`, `suspend`, `mcp_reply`, `status`, `refuse`), versioned. Stopped on purpose = a `stopped` file next to the socket.
Not done: backends are still started and held by each claude++ itself (a supervisor crash does not kill them, and the windows fall
back to their own budget check); the budget does not yet weigh a tab's cost to resume against its memory.
Measured with the real binary (`start → two instances → one dies without a goodbye`): the dead instance's backend was stopped 10.3 s
later, the instance that said goodbye was left alone, and the supervisor exited 20 s after that clean-up had finished. For the budget,
two simulated windows (A: budget 200 MB, tabs of 80 and 70 MB; B: no budget, 120 MB) were 270 MB over: 6.3 s in, A was asked to stop
its longest-idle tab, and B was never asked.

## Many agents, one process

To run several agents (lanes) at once, let the model use Claude Code's own `Agent` tool. A subagent lives
inside the tab's `claude` process; a lane that is another `claude -p` process costs a whole backend.
Measured with `python3 bench/lanes.py <arm> [N]` (haiku, macOS `footprint` summed over the process tree):

| 4 agents running at once | footprint |
|---|---|
| 1 process, no subagents | 252 MB |
| **1 process, 4 parallel subagents** | **268 MB** (peak 276), about +4 MB per agent |
| 4 separate `claude` processes | **936 MB**, about 234 MB per agent |
| `lean` + `Agent`, 6 subagents that read the repo with Bash | 134 → 166 MB peak, about +5 MB per agent |

So a lane gets no cheaper as a process: it is already about 50× cheaper in-process. Going below ~5 MB would mean
running the model loop and tools inside claude++, which conflicts with "Claude Code does everything agentic" and
"no hidden LLM calls", for a saving of a few MB per agent. claude++ therefore adds no spawn tool of its own; the
memory work is in keeping `Agent` available and not starting extra backends.

`lean` has no `Agent`. Adding it costs about **2.3k prompt tokens per request** (9.3k → 11.6k, mostly cached).
A one-line profile does it, `~/.config/claude++/profiles/lanes.json`:

```json
{ "extends": "lean", "tools": "Bash,Read,Edit,Write,AskUserQuestion,WebFetch,WebSearch,Agent" }
```

then `claude++ --profile lanes` or `/new lanes`. Subagent steps already render under their `Agent` call, and a
tab with a subagent still running (foreground, or backgrounded, which is Claude Code's default) is never stopped by `--mem-budget` or idle suspension.

Caveat: trivial and file-reading subagents were measured, not long-running ones whose context grows. A model
with Bash can still start `claude -p` itself, which this does not prevent.

## Benchmark (`bench/run.py`)

Controlled, per-process-tree measurements of the headless backend's configurations (haiku, one factor changed per
config, shuffled runs, warm-ups discarded, a clean environment). Latest run: Claude Code 2.1.312, 2026-10-28,
`python3 bench/run.py --task-reps 3 --workdir <empty dir>`; full table in `bench/results/2026-10-28-haiku.txt`.

| config | tokens, fresh request | process tree | cost per task (bugfix / implement / rename) | passed |
|---|---|---|---|---|
| default | 30.4k | 243 MB | $0.0062 / $0.0061 / $0.0075 | 9/9 |
| `--setting-sources project,local` | 19.5k | 141 MB | $0.0032 / $0.0034 / $0.0036 | 9/9 |
| lean `--tools` set | 11.6k | 148 MB | $0.0025 / $0.0024 / $0.0025 | 9/9 |
| `lean` (both) | 9.3k | 132 MB | $0.0017 / $0.0018 / $0.0019 | 9/9 |
| `lean` + a short custom system prompt | 6.3k | 133 MB | $0.0010 / $0.0010 / $0.0013 | 9/9 |

The tasks are small (fix a bug, implement a function, rename across files, each checked by its tests), too easy to
show that a custom system prompt does not hurt quality on real work, so `lean` is the shipped profile and the
custom prompt stays opt-in. The machine was shared and busy (load 5–20, recorded per run), which affects the
timings in the full table, not tokens or memory. Use footprint, not RSS: RSS overstated memory about 2×.

## Limitations / not yet done

- Resume history is read from Claude Code's on-disk transcript (an internal format, parsed leniently and
  skipping what it does not recognise). Only the last 400 messages are replayed, to keep memory flat.
- Inline images need a kitty-graphics terminal with unicode placeholders (kitty, Ghostty). Not shown for
  replayed history. Terminals that report no cell size get a 1:2 cell-shape guess, so aspect can be slightly off.
- The list reads background sessions from the supervisor's state files (an internal format, read leniently),
  searches titles, summaries and first prompts rather than full text, and cannot rename.
- Markdown: no nested block quotes inside lists, no HTML, no setext
  (underlined) headings, and a soft line break stays a line break rather than joining the paragraph.
- The `@` picker indexes up to 100k paths when it opens (cached for 30 s); a very large monorepo may feel it.
  Paths with spaces are not completed. The model picker shows every model Claude Code lists, including ones
  you may not have access to.
- Shift+Enter needs a terminal with the kitty keyboard protocol; alt+enter and ctrl+j always work.
- A backend that dies before its first message twice in a row (a bad flag, a deleted conversation) is no longer
  started by typing; sending a message tries again. A conversation `claude` cannot find is forgotten, and the next
  message starts a new one.
- Tested by hand on macOS only (Linux: compiles and passes the tests in CI; Windows: not supported). macOS tools used where present: `footprint` (memory), `sips` (image conversion), `osascript`
  (clipboard images) and `pbcopy`; elsewhere memory falls back to `ps` RSS and images need `wl-paste`/`xclip`.

## Confidence checks

Run `cargo build --release` first. All use the real `claude` or none at all; none touches your settings.

| Command | Checks | Cost |
|---|---|---|
| `cargo test` | 322 unit and end-to-end tests (5 more are ignored by default) (the pty ones skip themselves where no pty can be opened) against a fake `claude`; includes a light fuzz run | free, 5 s |
| `git config core.hooksPath .githooks` | once per clone: `git push` then runs `cargo test` and `bench/budget.py` first (`git push --no-verify` skips it). The contract check calls the real `claude`, so it stays manual | free, ~2 min |
| `FUZZ_ROUNDS=40 FUZZ_SEED=7 cargo test --release hostile_backend` | the UI against hostile backend messages (mutated real ones: wrong types, huge values, control characters), random keys, read-only panels, terminals from 1×1 to 300×4. Run it in a debug build too: that is where integer overflow shows. Found: an overflow on absurd token counts, now capped at the source (`backend::count`) | free |
| `FUZZ_FILES=600 cargo test hostile_transcript` | resume and every history reader against corrupt transcripts (cut-off lines, invalid UTF-8, a 3 MB line) | free |
| `python3 bench/contract.py` | the installed `claude` still behaves as claude++ assumes, including the token saving | cents |
| `python3 bench/budget.py` | resting claude++ stays tiny: binary, memory, first paint, idle CPU | free |
| `python3 bench/hostile.py` | SIGTERM/SIGHUP/SIGKILL leave no orphan `claude` and a restored terminal; a backend killed mid-turn; Esc interrupts; a permission prompt can be declined. `HOSTILE_CONFIG=user` uses your own config | cents, 3 min |
| `python3 bench/share.py` | three tabs, three MCP servers, sharing off vs on: server processes started, who answered each tool call, memory, and that nothing is left after a quit or a SIGKILL | cents |
| `python3 bench/editors.py` | real vim, nvim and nano inside claude++: editor-mode send, ctrl+g full screen, ctrl+\\ cancel; a fake `claude` logs exactly what it receives | free |
| `python3 bench/soak.py` | 3,000 turns cost 23 MB (about 4 KB per turn, linear), echo stays near 1 ms, idle CPU about 0; a single 30 MB tool result costs 191 MB and stays responsive | free |
| `python3 bench/away.py` | the front tab gives its memory back when you leave, and the conversation survives | cents |

## Tests

- `cargo test`: protocol state machine (streaming → final reconciliation, tools/results,
  permission queue, questions, interrupt, subagent nesting), wrapping / CJK width / cursor
  math, diff rendering, base64, and backend process lifecycle against a fake `claude` script
  (round trip, stderr capture, clean exit, kill on hang). `.github/workflows/ci.yml` runs `cargo test` and `clippy -D warnings` on macOS and Linux (not yet run: no remote).
- The supervisor tests (`supervisor::tests`) use Unix sockets and real child processes, so they fail inside a sandbox that forbids
  creating sockets (Operation not permitted); run them outside it.
- End-to-end runs were driven through a pty plus a terminal emulator against the real backend:
  permission → file written, multi-turn recall, tabs, interrupt, AskUserQuestion, image
  attachment, idle suspend → resume with context, and quit with no orphans.
