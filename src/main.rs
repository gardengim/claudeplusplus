mod backend;
mod config;
mod explorer;
mod jobs;
mod lanes;
mod profile;
mod instructions;
mod optimize;
mod permissions;
mod tabcfg;
mod usage;
mod files;
mod history;
mod image;
mod markdown;
mod pane;
mod panel;
mod prompt;
mod queue;
mod schedule;
mod search;
mod vim;
mod session;
mod cc_keys;
mod context;
mod settings;
mod statusline;
mod theme;
mod ui;

use backend::{Backend, Launch, Msg};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, MouseButton, MouseEventKind, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use serde_json::json;
use session::{Choice, Cmd, Entry, ModelInfo, Session};
use std::path::PathBuf;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub enum AppEvent {
    Term(Event),
    /// (session id, backend pid, message)
    Backend(u64, u32, Msg),
    /// A status line run finished: session id, run generation, and the output lines (None = failed/cancelled).
    Status(u64, u64, Option<Vec<String>>),
    /// `claude stop` finished: the message to show, and the session to open afterwards if it worked.
    Stopped(Result<String, String>, Option<(String, String, Option<String>)>),
    /// Output from (or the end of) the editor pane with this id.
    Pane(u64, pane::Msg),
    /// SIGTERM or SIGHUP: leave through the normal path so the terminal is restored.
    Quit,
    /// The clipboard was read off the UI thread: session id, the PNG if there was one, what to say if not.
    Clipboard(u64, Option<Vec<u8>>, &'static str),
    /// A `!command` finished: tab id, transcript entry id, command, what it printed.
    Shell(u64, String, String, std::io::Result<std::process::Output>),
    /// `--rewind-files` finished: tab id, what to say.
    Rewound(u64, Result<String, String>),
    /// The `@` index for a directory was built off the UI thread.
    Files(String, Vec<String>),
    /// Memory sampled off the UI thread: footprint KB per backend pid, and our own.
    Mem(Vec<(u32, u64)>, u64),
    /// The terminal's background turned light (true) or dark while running.
    Theme(bool),
}

/// `--profile lean` = this tool set, no MCP, and no user-level settings (user CLAUDE.md,
/// skills, hooks). Measured on Claude Code 2.1.309 in this repo: 65k → 7k prompt tokens.
use tabcfg::Row;

const HELP: &str = "\
claude++ — a lighter native frontend for headless Claude Code

USAGE: claude++ [options] [-- <extra claude args>]

  --profile <name>            profile for new tabs: full|sources|lean or one of yours (see PROFILES) (change per tab with /profile, /new <profile>)
                        sources = project+local settings only; lean = that plus core tools, no MCP
  --sources <list>      which settings to load: user,project,local (default all). Excluding
                        `user` drops your ~/.claude CLAUDE.md, skills, hooks, plugins
  --tools <list>        built-in tools to offer, e.g. Bash,Read,Edit,Write
  --no-mcp              ignore all MCP servers
  --env KEY=VAL         set an env var on every claude process (repeatable), e.g.
                        BUN_JSC_gcMaxHeapSize=100000000
  --model <m>           model alias or id
  --permission-mode <m> default | acceptEdits | plan | auto | ...
  -c, --continue        continue the latest conversation in this directory
  -r, --resume <id>     resume a session by id
  --idle-suspend <s>    stop idle background tabs' claude after s seconds (0 = never, default 120)
  --mem-budget <MB>     when all claude processes together exceed this footprint, stop the least
                        recently used idle tab (0 = off, default). Busy/focused tabs are never stopped
  --cache-ttl <min>     prompt-cache lifetime used for the status-line countdown (default 60; use 5
                        if your account only gets the short cache)
  --statusline <v>      which status line to show: `claude` runs the `statusLine` command from your Claude Code
                        settings (rich, but a process per refresh), `builtin` is claude++'s own line only (nothing
                        spawned), or a shell command to run instead. Default: what /statusline last saved in
                        ~/.config/claude++/config.json, else builtin
  --claude-bin <path>   claude executable (default: claude)

KEYS  enter send · shift+enter, alt+enter or ctrl+j newline · esc/ctrl+c interrupt
      esc esc on an empty input: rewind to an earlier message (conversation, files, or both)
      ctrl+t new tab · ctrl+n/ctrl+p next/prev tab · alt+1..9 jump · ctrl+d close tab
      shift+tab permission mode · ctrl+o expand all tool output · alt+o or a click folds one call · wheel or pgup/pgdn scroll · ctrl+end jump to latest
      ctrl+g edit the message in your editor, full screen inside claude++ (ctrl+\\ cancels) · ctrl+b move a running command to the background (/tasks stops it)
      alt+q manage queued messages (reorder, send now, edit) · ctrl+f search the conversation or what you sent
      /editor: the message area is your editor, always open (quit it to send; ctrl+\\ cancels; no / or @ completion).
              Still claude++'s: pgup/pgdn, alt+1..9, shift+tab, ctrl+t/o, alt+f/r/d/b (as ctrl+f/r/d/b), alt+←/→ tabs, and until you type: ↑↓ history, ← sessions, esc
      /vim: modal keys in the box (esc normal mode; i a o insert; hjkl w b e f t d c y p u v; / searches); /config → Input mode
      ctrl+r search the messages you sent · ← at the start of the input: earlier sessions · tab completes a /command or an @file (↑↓ to pick, esc hides)
      ctrl+v paste an image as [Image #N] at the cursor (delete the token to drop it)
COMMANDS  /usage (/cost /status) /memory [user] /export [file] /tasks /context /queue /schedule /search /vim /find /session /new [profile] /profile [name] [--hard] /close /clear /resume [id] /model [m] /effort [level] /mode <m>
          /config (/setting) /statusline [claude|builtin] /img <path> /quit
          anything else starting with / goes to Claude Code (e.g. /compact)
          !<command> runs a shell command here (no model turn); its output goes with your next message
@FILES    type @ to pick a file or folder; Claude Code reads what you mention (images included)
PROFILES  your own: ~/.config/claude++/profiles/<name>.json (system prompt, tools, MCP, model, env...; see README)
EDITING   alt+←/→ word · alt+backspace / ctrl+w delete word · ctrl+u / ctrl+k cut to line start / end · ↑↓ move between rows, then history · ctrl+z or ctrl+_ undo, ctrl+shift+z or alt+z redo · alt+p model picker · ctrl+l repaint · ctrl+y copies the text selected with the mouse, else the last answer (also /copy [code])
IMAGES    ctrl+v, drag a file into the window or /img <path> to attach; pictures in the transcript are drawn
          inline in kitty/Ghostty (CLAUDEPP_IMAGES=on|off overrides detection)";

/// Commands claude++ handles itself: (name, argument hint, description).
const LOCAL_COMMANDS: [(&str, &str, &str); 32] = [
    ("new", "[profile]", "open a new tab"),
    ("profile", "[name] [--hard]", "show or change this tab's profile"),
    ("close", "", "close this tab"),
    ("clear", "", "start a fresh conversation in this tab"),
    ("resume", "[id]", "browse and reopen earlier sessions"),
    ("sessions", "", "list sessions: this directory's and background ones (also ←)"),
    ("model", "[name]", "pick a model and effort, or switch directly"),
    ("effort", "[level]", "pick or set the thinking effort"),
    ("color", "[name]", "pick this conversation's colour: the tab, frames and highlights wear it"),
    ("mode", "[mode]", "set the permission mode (no argument: this tab's mode and rules)"),
    ("memory", "[user]", "edit this project's CLAUDE.md (or your own ~/.claude/CLAUDE.md) in $EDITOR"),
    ("export", "[file]", "save this conversation as text (default: a dated file here)"),
    ("img", "<path>", "attach an image to the next message"),
    ("find", "<text>", "jump to the newest match in the conversation; again for older"),
    ("queue", "", "messages waiting for Claude: reorder, send now, edit, delete (alt+q)"),
    ("schedule", "<when> <message>", "send a message later: 10m, 2h30m or a time like 14:30"),
    ("search", "[text]", "search this conversation or the messages you sent (ctrl+f)"),
    ("vim", "", "turn vim keys in the message box on or off"),
    ("editor", "", "turn the message box into your $EDITOR on or off (quit the editor to send)"),
    ("copy", "[code]", "copy the last answer, or its last code block, to the clipboard"),
    ("config", "", "settings: claude++'s and Claude Code's, by section, searchable"),
    ("setting", "", "same as /config"),
    ("session", "", "this tab's permission mode and tool rules"),
    ("usage", "", "where this tab's context goes, and your plan limits"),
    ("context", "", "the context window drawn: what fills it, category by category"),
    ("tasks", "", "this tab's subagents: state, steps and report"),
    ("optimize", "[light|balanced|aggressive|off|now]", "trim this project's start-up context from evidence (reversible)"),
    ("permissions", "", "saved allow/ask/deny rules from your settings files: add and delete"),
    ("instructions", "", "tick CLAUDE.md files and sections in or out of new sessions"),
    ("statusline", "[claude|builtin]", "choose which status line to show"),
    ("help", "", "keys and commands"),
    ("quit", "", "leave claude++"),
];

/// One row of the `/command` and `@file` popup.
#[derive(Clone)]
pub struct Completion {
    /// `/` for commands, `@` for files.
    pub prefix: char,
    pub name: String,
    pub hint: String,
    pub desc: String,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub active: usize,
    next_id: u64,
    pub input: String,
    /// Earlier states of the message box for ctrl+z: (text, cursor).
    undo: Vec<(String, usize)>,
    /// What ctrl+z took off, for ctrl+shift+z (alt+z) to put back; any new edit forgets it.
    redo: Vec<(String, usize)>,
    /// Columns the message box wraps at, as last drawn: ↑/↓ move by the rows it shows.
    pub input_w: usize,
    /// What was typed before ↑ first recalled history, for ↓ to bring back.
    hist_draft: String,
    last_edit: Instant,
    reset_undo: bool,
    close_armed: Option<Instant>,
    /// Cursor position in chars.
    pub cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    pub expanded: bool,
    launch: Launch,
    pub cwd: String,
    tx: Sender<AppEvent>,
    quit_armed: Option<Instant>,
    quit: bool,
    pub flash: Option<String>,
    flash_at: Instant,
    pub ui_mem_kb: u64,
    last_sample: Instant,
    idle_suspend: Duration,
    mem_budget_kb: u64,
    budget_warned: Option<Instant>,
    pub explorer: Option<explorer::Explorer>,
    /// Where the last frame put the tabs and lists, for mouse clicks.
    pub hits: ui::Hits,
    /// Config "sessions = panel": ← opens a docked list first, a second ← the full screen.
    pub dock_mode: bool,
    /// `--profile` was given: a project's own profile does not override it.
    profile_forced: bool,
    /// off | bell | desktop: how a background tab gets your attention.
    notify: String,
    /// What `notify` emitted, kept for tests.
    #[cfg(test)]
    sent_notices: Vec<String>,
    /// Real terminal attached: only then are mouse-mode escapes written.
    tty: bool,
    /// The title last written to the terminal.
    term_title: String,
    mouse: bool,
    /// Copy-on-select: a mouse selection is copied as soon as the button is released (else ctrl+y copies it).
    autocopy: bool,
    /// The text selected with the mouse: where the drag started and where it ended (screen cells). It stays
    /// until the next click, key or scroll.
    drag: Option<((u16, u16), (u16, u16))>,
    /// The button is still down.
    dragging: bool,
    /// The conversation's cells as last drawn while a drag was active, for the text to copy.
    snap: Vec<Vec<String>>,
    pub dock: Option<explorer::Explorer>,
    pub dock_focus: bool,
    /// Session-list preferences shared by the side panel and the full list, and saved: grouping, folded groups.
    list_prefs: (u8, Vec<String>),
    /// The main area shows the side panel's highlighted stored conversation instead of the tab.
    pub dock_peek: bool,
    dock_at: Instant,
    /// Slash commands Claude Code offers, remembered across runs so completion works before any
    /// backend is running.
    known_commands: Vec<Cmd>,
    /// Highlighted row of the `/command` and `@file` popup.
    pub sel: usize,
    /// Models Claude Code offers, remembered like the commands.
    known_models: Vec<ModelInfo>,
    pub model_picker: Option<ModelPicker>,
    /// `/color` picker: the highlighted row (0 = default, then the palette in order).
    pub color_picker: Option<usize>,
    pub rewind: Option<RewindPicker>,
    /// First Esc of a double-Esc rewind.
    esc_armed: Option<Instant>,
    pub settings: Option<settings::Ui>,
    pub tabcfg: Option<tabcfg::Ui>,
    pub instr: Option<instructions::Ui>,
    pub perms: Option<permissions::Ui>,
    pub opt: Option<optimize::Ui>,
    pub usage_open: bool,
    /// `/tasks`: the tab's subagents.
    pub lanes: Option<lanes::Ui>,
    pub ctx_ui: Option<context::Ui>,
    /// `/queue`: the tab's waiting messages.
    pub queue_ui: Option<queue::Ui>,
    /// `/schedule`: the time and message being set.
    pub sched_ui: Option<schedule::Ui>,
    /// A time was picked: what is typed in the message box next is scheduled for it instead of sent.
    pub sched_at: Option<SystemTime>,
    /// Search as you type (ctrl+f).
    pub search: Option<search::Ui>,
    /// Vim keys in the message box (`/vim`).
    pub vim: vim::Vim,
    /// Messages sent while Claude works wait in the queue (else they go at once).
    pub queue_on: bool,
    /// ctrl+g was pressed: the main loop opens the editor.
    edit_request: bool,
    /// Input mode "editor": the message box is your editor, running in a pane; quitting it sends the message.
    pub editor_mode: bool,
    /// The running editor: in the box's place (input mode editor) or full screen (ctrl+g).
    pub pane: Option<pane::Pane>,
    pane_seq: u64,
    /// Cursor shape last asked of the terminal for the editor (DECSCUSR), so only changes are written.
    pane_shape: u8,
    /// ctrl+g: the box as it was, to put back if the editor fails.
    pane_old: Option<(String, usize)>,
    /// Opening the editor failed (no pty): stop trying and use the plain box.
    pane_failed: bool,
    /// Nothing has been sent to the editor since it opened: ↑ ↓ ← and Esc are still claude++'s.
    pane_typed: bool,
    /// `editor` in config.json: a command that overrides $VISUAL and $EDITOR.
    pub editor_cmd: Option<String>,
    /// ctrl+l: clear and repaint the whole screen before the next frame.
    clear_request: bool,
    /// `/memory`: a file to open in the editor before the next frame.
    edit_file: Option<std::path::PathBuf>,
    /// The cursor shape last asked of the terminal, so it is sent only when the mode changes.
    cursor_mode: Option<vim::Mode>,
    /// File index for `@`, rebuilt when the popup opens and the list is older than 30 s.
    files: Vec<String>,
    files_at: Option<Instant>,
    file_rows: Vec<Completion>,
    file_key: Option<String>,
    /// The input text for which the user hid the popup with Esc.
    comp_off: Option<String>,
    clip_n: usize,
    /// The user's status line command (None if not configured or turned off with the flag).
    statusline: Option<statusline::Config>,
    pub statusline_on: bool,
    statusline_mode: statusline::Mode,
    /// claude++'s own config file, where `/statusline claude|builtin` is saved.
    config_path: Option<PathBuf>,
    claude_paths: statusline::Paths,
    /// Last key or paste; the periodic refresh stops after ten idle minutes.
    last_input: Instant,
    /// Which tab the status line was last refreshed for, to refresh on a tab switch.
    status_for: u64,
    /// Said once in the first tab at startup (e.g. a status line skipped for lack of trust).
    startup_note: Option<String>,
    default_profile: String,
    pub cache_ttl: Duration,
}

/// Selection state of the double-Esc rewind picker: a message, and what to restore (an index into
/// `RewindPicker::choices`).
#[derive(Clone, Copy)]
pub struct RewindPicker {
    pub sel: usize,
    pub restore: usize,
}

impl RewindPicker {
    /// (conversation, code) for each restore choice; code only when files were edited since.
    pub fn choices(edited: bool) -> &'static [(&'static str, bool, bool)] {
        if edited {
            &[("conversation and code", true, true), ("conversation", true, false), ("code", false, true)]
        } else {
            &[("conversation", true, false)]
        }
    }
}

/// Selection state of the `/model` picker: a row in the model list and a level in its effort row.
#[derive(Clone, Copy)]
pub struct ModelPicker {
    pub sel: usize,
    pub effort: usize,
    /// Fast mode wanted (only applies if the highlighted model supports it).
    pub fast: bool,
}

struct Opts {
    launch: Launch,
    resume: Option<String>,
    cont: bool,
    idle: Option<u64>,
    mem_budget_mb: Option<u64>,
    profile: String,
    cache_ttl_min: Option<u64>,
    statusline: String,
}

fn profile_args(p: &str) -> Result<Vec<String>, String> {
    profile::spec(p).map(|s| s.args)
}

/// `profile` from `<dir>/.claude++/config.json`, if the project has one and it names a profile that exists.
fn project_profile(dir: &str) -> Option<String> {
    let file = std::path::Path::new(dir).join(".claude++").join("config.json");
    config::get_str(&file, "profile").filter(|p| profile_args(p).is_ok())
}

/// A full-profile tab above this is worth suggesting the lean profile for.
const HEAVY_TAB_KB: u64 = 400 * 1024;

static ENHANCED: AtomicBool = AtomicBool::new(false);
/// Set while an external editor owns the terminal: the reader thread must leave its keys alone.
static EDITING: AtomicBool = AtomicBool::new(false);

/// `text` edited in `editor` (a shell command, so `code -w` works) through a private temp file; the editor's
/// last line break is not part of the message. Err when the editor could not run or failed.
fn edit_text(editor: &str, text: &str) -> io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!("claudepp-message-{}-{}.md", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    f.write_all(text.as_bytes())?;
    drop(f);
    let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status();
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    let status = status?;
    if !status.success() {
        return Err(io::Error::other(format!("{editor} {status}")));
    }
    let edited = edited?;
    Ok(edited.strip_suffix('\n').unwrap_or(&edited).to_string())
}

/// The editor to use: `$VISUAL`, else `$EDITOR`, else `vi`.
fn editor_command() -> String {
    ["VISUAL", "EDITOR"].iter().filter_map(|k| std::env::var(k).ok()).find(|v| !v.trim().is_empty()).unwrap_or_else(|| "vi".into())
}

/// Give the terminal to a program (the editor) for the length of `f`, then take it back and repaint.
fn with_terminal_lent<T>(term: &mut ratatui::DefaultTerminal, app: &mut App, f: impl FnOnce() -> T) -> T {
    EDITING.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(120)); // the reader thread is out of its read by now
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[0 q");
    if ENHANCED.load(Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste);
    ratatui::restore();
    let res = f();
    // Not `ratatui::init()`: that installs another panic hook each time, one that restores the terminal
    // on a worker thread's panic too.
    let _ = terminal::enable_raw_mode();
    let _ = execute!(io::stdout(), terminal::EnterAlternateScreen);
    let mut out = io::stdout();
    let _ = execute!(out, EnableBracketedPaste);
    if ENHANCED.load(Ordering::Relaxed) {
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    if app.mouse {
        app.set_mouse(true);
    }
    app.cursor_mode = None;
    let _ = term.clear();
    EDITING.store(false, Ordering::Relaxed);
    res
}

/// ctrl+g: hand the terminal to the editor with the message in it, take the result back into the box.
fn edit_in_editor(term: &mut ratatui::DefaultTerminal, app: &mut App) {
    let editor = app.editor();
    let old = (app.input.clone(), app.cursor);
    // Preferably inside claude++ (the conversation keeps updating); the terminal is only lent when that cannot start.
    match app.open_pane(pane::Use::Edit, &old.0) {
        Ok(()) => return app.pane_old = Some(old),
        Err(e) => app.flash(format!("embedded editor unavailable ({e}): opening it the plain way")),
    }
    let res = with_terminal_lent(term, app, || edit_text(&editor, &old.0));
    match res {
        Ok(text) if text != old.0 => {
            app.undo.push(old);
            app.redo.clear();
            app.set_input(text);
            app.vim.mode = vim::Mode::Insert;
            app.flash("message edited");
        }
        Ok(_) => app.flash("message unchanged"),
        Err(e) => app.flash(format!("editor: {e}; your message is as it was (set $VISUAL or $EDITOR)")),
    }
    app.vim_shape();
}

/// `/memory`: edit a CLAUDE.md in place. Claude Code watches it, so the next turn reads the change.
fn edit_file_in_editor(term: &mut ratatui::DefaultTerminal, app: &mut App, path: &std::path::Path) {
    let editor = app.editor();
    let status = with_terminal_lent(term, app, || std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path).status());
    match status {
        Ok(s) if s.success() => app.flash(format!("saved {}", path.display())),
        Ok(s) => app.flash(format!("editor: {editor} {s}")),
        Err(e) => app.flash(format!("editor: {e} (set $VISUAL or $EDITOR)")),
    }
}

/// Put the terminal back the way we found it: mouse reporting off, paste and key modes reset, images
/// deleted, raw mode and the alternate screen left. Safe to call twice (the panic hook and normal exit).
fn restore_terminal() {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[0 q");
    if ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste);
    image::clear_all();
    let _ = write!(out, "\x1b[23;0t"); // give the terminal its own title back
    ratatui::restore();
}

/// The body of the last ``` fenced block in `text`.
fn last_code_block(text: &str) -> Option<String> {
    let mut blocks = vec![];
    let mut cur: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match cur.take() {
                Some(b) => blocks.push(b.join("\n")),
                None => cur = Some(vec![]),
            }
        } else if let Some(b) = cur.as_mut() {
            b.push(line);
        }
    }
    blocks.pop()
}

fn parse_args() -> Result<Opts, String> {
    let mut a = std::env::args().skip(1);
    let mut o = Opts {
        // Headless Claude Code keeps no file checkpoints unless asked; a rewind restores code from them.
        launch: Launch { bin: "claude".into(), env: vec![("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".into(), "1".into())], ..Default::default() },
        resume: None,
        cont: false,
        idle: None,
        mem_budget_mb: None,
        profile: String::new(),
        cache_ttl_min: None,
        statusline: String::new(),
    };
    let mut profile = String::new(); // empty: not given, so the saved or project choice applies
    let mut extra = vec![];
    let need = |a: &mut std::iter::Skip<std::env::Args>, f: &str| a.next().ok_or(format!("{f} needs a value"));
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(HELP.into()),
            "--profile" => profile = need(&mut a, &arg)?,
            "--sources" | "--tools" => {
                let flag = if arg == "--sources" { "--setting-sources".to_string() } else { arg.clone() };
                extra.push(flag);
                extra.push(need(&mut a, &arg)?);
            }
            "--no-mcp" => extra.push("--strict-mcp-config".into()),
            "--env" => {
                let kv = need(&mut a, &arg)?;
                let (k, v) = kv.split_once('=').ok_or("--env needs KEY=VALUE")?;
                o.launch.env.push((k.into(), v.into()));
            }
            "--model" | "--permission-mode" => {
                extra.push(arg.clone());
                extra.push(need(&mut a, &arg)?);
            }
            "-c" | "--continue" => o.cont = true,
            "-r" | "--resume" => o.resume = Some(need(&mut a, &arg)?),
            "--idle-suspend" => o.idle = Some(need(&mut a, &arg)?.parse().map_err(|_| "bad --idle-suspend")?),
            "--mem-budget" => o.mem_budget_mb = Some(need(&mut a, &arg)?.parse().map_err(|_| "bad --mem-budget")?),
            "--cache-ttl" => o.cache_ttl_min = Some(need(&mut a, &arg)?.parse().map_err(|_| "bad --cache-ttl")?),
            "--statusline" => o.statusline = need(&mut a, &arg)?,
            "--claude-bin" => o.launch.bin = need(&mut a, &arg)?,
            "--" => extra.extend(a.by_ref()),
            other => return Err(format!("unknown option {other}\n\n{HELP}")),
        }
    }
    profile_args(if profile.is_empty() { "full" } else { &profile })?; // validate; each tab applies its own profile at launch
    // Bypass joins the shift+tab cycle when the backend is allowed to bypass (flags after `--`).
    let bypass = extra.iter().any(|a| a.contains("dangerously-skip-permissions")) || extra.windows(2).any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions");
    tabcfg::BYPASS.store(bypass, std::sync::atomic::Ordering::Relaxed);
    o.profile = profile;
    o.launch.args = extra;
    Ok(o)
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(msg) => {
            println!("{}", theme::keys(&msg));
            std::process::exit(if msg == HELP { 0 } else { 2 });
        }
    };
    if let Err(e) = run(opts) {
        eprintln!("claude++: {e}");
        std::process::exit(1);
    }
}

fn run(opts: Opts) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    // Every setting: a flag wins for this run, else what /config saved, else the built-in default.
    let config_path = config::path();
    let saved = |key: &str| config_path.as_deref().and_then(|p| config::get_str(p, key));
    let saved_num = |key: &str| saved(key).and_then(|v| v.parse::<u64>().ok());
    let idle = opts.idle.or(saved_num("idle_suspend_s")).unwrap_or(120);
    let mut app = App::new(opts.launch, cwd, tx.clone(), Duration::from_secs(idle));
    app.config_path = config_path.clone();
    app.mem_budget_kb = opts.mem_budget_mb.or(saved_num("mem_budget_mb")).unwrap_or(0) * 1024;
    app.profile_forced = !opts.profile.is_empty();
    let profile = Some(opts.profile).filter(|p| !p.is_empty()).or_else(|| saved("profile").filter(|p| profile_args(p).is_ok()));
    if let Some(p) = profile {
        app.default_profile = p;
    }
    app.cache_ttl = Duration::from_secs(opts.cache_ttl_min.or(saved_num("cache_ttl_min")).unwrap_or(60) * 60);
    image::set_mode(&saved("images").unwrap_or_default());
    if let Some(p) = config_path.as_deref() {
        let folds = config::get_value(p, "session_folds").and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()));
        let group = saved("session_group").and_then(|g| g.parse().ok()).unwrap_or(0);
        explorer::set_group(group);
        app.list_prefs = (explorer::group(), folds.unwrap_or_default());
    }
    app.dock_mode = saved("side_panel").as_deref() == Some("on");
    // "vim": "on" is what older configs say.
    let mode = saved("input_mode").unwrap_or_else(|| if saved("vim").as_deref() == Some("on") { "vim".into() } else { "default".into() });
    app.vim.on = mode == "vim" || mode == "helix"; // "helix" was an input mode once: its users get vim
    app.editor_mode = mode == "editor";
    app.editor_cmd = saved("editor").filter(|c| !c.trim().is_empty());
    app.autocopy = saved("autocopy").as_deref() == Some("on");
    app.queue_on = saved("queue").as_deref() != Some("off");
    if let Some(n) = saved("notify").filter(|n| ["off", "bell", "desktop"].contains(&n.as_str())) {
        app.notify = n;
    }
    let mode = statusline::Mode::parse(&opts.statusline).or_else(|| saved("statusline").as_deref().and_then(statusline::Mode::parse));
    app.startup_note = app.apply_statusline_mode(mode.unwrap_or(statusline::Mode::Builtin));
    app.known_commands = load_commands();
    app.history = history::sent_prompts(&app.cwd);
    app.known_models = cache_path("models.json").map(|p| read_models(&p)).unwrap_or_default();
    app.new_tab();
    if let Some(n) = app.startup_note.take() {
        app.sessions[0].note(n);
    }
    if let Some(id) = opts.resume {
        app.sessions[0].resume(id);
    } else if opts.cont {
        let cwd = app.cwd.clone();
        app.sessions[0].continue_last(&cwd);
    }

    if let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP]) {
        let tx = tx.clone();
        thread::spawn(move || {
            for _ in signals.forever() {
                let _ = tx.send(AppEvent::Quit);
            }
        });
    }
    let mut term = ratatui::init();
    theme::set_mode(saved("theme").as_deref().unwrap_or("auto"));
    if let Some(light) = theme::query_light_background() {
        theme::set_light(light);
    }
    app.tty = true;
    app.mouse = saved("mouse").as_deref() != Some("off");
    if app.mouse {
        app.set_mouse(true);
    }
    let mut out = io::stdout();
    let _ = execute!(out, EnableBracketedPaste);
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        // Lets Shift+Enter be told apart from Enter.
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        ENHANCED.store(true, Ordering::Relaxed);
    }
    // A crash must not leave the terminal reporting the mouse or swallowing paste: undo everything first.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only the UI thread's panic ends the program; a worker's must not leave the UI drawing into a cooked terminal.
        if thread::current().name() == Some("main") {
            restore_terminal();
        }
        previous(info);
    }));
    thread::spawn(move || {
        let (mut last_input, mut last_check) = (Instant::now(), Instant::now());
        loop {
            // Polling, not blocking in read: an editor started with ctrl+g must get the keys, not this thread.
            if EDITING.load(Ordering::Relaxed) || !event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if EDITING.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                // The terminal may switch light/dark under us (macOS appearance at sunset). Its notification of
                // that would wedge crossterm's parser, so ask again instead: from this thread, the only reader of
                // the terminal, and only while nobody types (a key arriving during the round trip would be
                // read as part of the reply), and only from a terminal that answered at startup.
                // ponytail: a key typed in the query's ~ms window is lost; a raw-input reader could re-inject it.
                if theme::follows_terminal() && last_input.elapsed() > Duration::from_secs(3) && last_check.elapsed() > Duration::from_secs(3) {
                    last_check = Instant::now();
                    if let Some(was) = theme::detected()
                        && let Some(now) = theme::query_light_background()
                        && now != was
                        && tx.send(AppEvent::Theme(now)).is_err()
                    {
                        break;
                    }
                }
                continue;
            }
            last_input = Instant::now();
            let Ok(e) = event::read() else { break };
            if tx.send(AppEvent::Term(e)).is_err() {
                break;
            }
        }
    });

    let res = (|| -> io::Result<()> {
        while !app.quit {
            term.draw(|f| ui::draw(f, &mut app))?;
            app.sync_title();
            let wait = if app.sessions.iter().any(|s| s.busy) { 100 } else { 1000 };
            let drawn = Instant::now();
            let mut typed = false;
            match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(ev) => {
                    typed = matches!(ev, AppEvent::Term(_) | AppEvent::Pane(..)); // the editor's echo of a key is drawn at once too
                    app.on_event(ev)
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Coalesce bursts (streaming deltas) into a single redraw, at most ~30 a second unless a key
            // arrives: a long reply is re-rendered per frame, so frames per delta would grow quadratic.
            let frame = Duration::from_millis(33);
            while !typed {
                let Some(left) = frame.checked_sub(drawn.elapsed()) else { break };
                match rx.recv_timeout(left) {
                    Ok(ev) => {
                        typed = matches!(ev, AppEvent::Term(_) | AppEvent::Pane(..)); // the editor's echo of a key is drawn at once too
                        app.on_event(ev);
                    }
                    Err(_) => break,
                }
            }
            while let Ok(ev) = rx.try_recv() {
                app.on_event(ev);
            }
            app.tick();
            if std::mem::take(&mut app.clear_request) {
                let _ = term.clear(); // ctrl+l: repaint everything, e.g. after another program wrote to the screen
            }
            if std::mem::take(&mut app.edit_request) {
                edit_in_editor(&mut term, &mut app);
            }
            if let Some(path) = app.edit_file.take() {
                edit_file_in_editor(&mut term, &mut app, &path);
            }
        }
        Ok(())
    })();

    restore_terminal();
    // EOF every backend first so they shut down in parallel, then wait for each.
    app.sessions.iter_mut().filter_map(|s| s.backend.as_mut()).for_each(Backend::close);
    drop(app);
    backend::wait_for_exits();
    res
}

impl App {
    fn new(launch: Launch, cwd: String, tx: Sender<AppEvent>, idle_suspend: Duration) -> Self {
        App {
            sessions: vec![],
            active: 0,
            next_id: 0,
            input: String::new(),
            undo: vec![],
            redo: vec![],
            input_w: 80,
            hist_draft: String::new(),
            last_edit: Instant::now(),
            reset_undo: false,
            close_armed: None,
            cursor: 0,
            history: vec![],
            hist_pos: None,
            expanded: false,
            launch,
            cwd,
            tx,
            quit_armed: None,
            quit: false,
            flash: None,
            flash_at: Instant::now(),
            ui_mem_kb: 0,
            last_sample: Instant::now() - Duration::from_secs(60),
            idle_suspend,
            mem_budget_kb: 0,
            budget_warned: None,
            explorer: None,
            hits: Default::default(),
            dock_mode: false,
            profile_forced: false,
            notify: "bell".into(),
            #[cfg(test)]
            sent_notices: vec![],
            tty: false,
            term_title: String::new(),
            mouse: true,
            autocopy: false,
            drag: None,
            dragging: false,
            snap: vec![],
            dock: None,
            dock_focus: false,
            list_prefs: (0, vec![]),
            dock_peek: false,
            dock_at: Instant::now(),
            known_commands: vec![],
            sel: 0,
            known_models: vec![],
            model_picker: None,
            color_picker: None,
            rewind: None,
            esc_armed: None,
            settings: None,
            tabcfg: None,
            instr: None,
            perms: None,
            opt: None,
            usage_open: false,
            lanes: None,
            ctx_ui: None,
            queue_ui: None,
            sched_ui: None,
            sched_at: None,
            search: None,
            vim: Default::default(),
            queue_on: true,
            edit_request: false,
            editor_mode: false,
            pane: None,
            pane_seq: 0,
            pane_shape: 0,
            pane_old: None,
            pane_failed: false,
            pane_typed: false,
            editor_cmd: None,
            clear_request: false,
            edit_file: None,
            cursor_mode: None,
            files: vec![],
            files_at: None,
            file_rows: vec![],
            file_key: None,
            comp_off: None,
            clip_n: 0,
            statusline: None,
            statusline_on: false,
            statusline_mode: statusline::Mode::Builtin,
            config_path: None,
            claude_paths: statusline::Paths::from_env(),
            last_input: Instant::now(),
            status_for: 0,
            startup_note: None,
            default_profile: "full".into(),
            cache_ttl: Duration::from_secs(3600),
        }
    }

    /// Start (or resume) the active tab's backend while the user is still typing, so its
    /// ~0.5 s startup overlaps with composing the message. Nothing is spawned for an idle UI,
    /// slash commands (once their list is known), or an empty input, and no API call is made until a
    /// message is sent.
    fn warm(&mut self) {
        // Typing `/` only starts a backend when completion still has no command list to offer.
        let wanted = !self.input.is_empty() && (!self.input.starts_with('/') || self.known_commands.is_empty());
        if wanted && self.sessions[self.active].backend.is_none() {
            let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
            self.s().warm(&launch, &tx);
        }
    }

    /// The terminal's title follows the active conversation (`claude++ · name`), written only when it changes.
    fn sync_title(&mut self) {
        let t = match self.sessions[self.active].title.as_str() {
            "" => "claude++".to_string(),
            n => format!("claude++ · {n}"),
        };
        if t != self.term_title {
            let first = self.term_title.is_empty();
            // Save the terminal's own title once so exit can restore it.
            let _ = write!(io::stdout(), "{}\x1b]2;{}\x07", if first { "\x1b[22;0t" } else { "" }, t.replace(|c: char| c.is_control(), ""));
            self.term_title = t;
        }
    }

    fn s(&mut self) -> &mut Session {
        &mut self.sessions[self.active]
    }

    pub fn placeholder(&self) -> &'static str {
        if !self.sessions[self.active].pending.is_empty() {
            "answer in the panel above…"
        } else if self.overlay_open() {
            "use the panel above…"
        } else if self.editor_mode && !self.pane_failed {
            "starting your editor…"
        } else {
            "message Claude…  (/help)"
        }
    }

    /// Launch settings for one tab: its profile's flags, then the global passthrough flags.
    fn launch_for(&self, i: usize) -> Launch {
        let mut l = self.launch.clone();
        let spec = profile::spec(&self.sessions[i].profile).unwrap_or_default();
        l.env.extend(spec.env);
        let mut args = spec.args;
        if self.sessions[i].unpinned {
            args.extend(["--system-prompt-snapshot".to_string(), "off".to_string()]);
        }
        args.append(&mut l.args);
        let mut settings = serde_json::Map::new();
        let cwd = self.sessions[i].cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let saved = optimize::load(&cwd);
        if let Some(o) = &saved {
            o.apply(&mut settings, &mut args);
        }
        if let (Some(f), Some(cache)) = (instructions::rules_path(), config::cache_dir()) {
            let rules = instructions::merge(&instructions::load_rules(&f), &saved.map(|o| o.instructions).unwrap_or_default());
            instructions::apply(&rules, &cache, &mut args, &mut settings);
        }
        if !settings.is_empty() {
            args.extend(["--settings".into(), serde_json::Value::Object(settings).to_string()]);
        }
        l.args = args;
        l.cwd = self.sessions[i].cwd.clone();
        l
    }

    /// Directory of the tab in front: what `@`, the status line and history are relative to.
    /// Some panel owns the keyboard (the input box is not the focus).
    fn overlay_open(&self) -> bool {
        self.explorer.is_some() || self.model_picker.is_some() || self.color_picker.is_some() || self.rewind.is_some() || self.settings.is_some() || self.tabcfg.is_some() || self.instr.is_some() || self.perms.is_some() || self.opt.is_some() || self.usage_open || self.ctx_ui.is_some() || self.lanes.is_some() || self.queue_ui.is_some() || self.sched_ui.is_some() || self.search.is_some()
    }

    fn cwd(&self) -> &str {
        self.sessions.get(self.active).and_then(|s| s.cwd.as_deref()).unwrap_or(&self.cwd)
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some(theme::keys(&msg.into()));
        self.flash_at = Instant::now();
    }

    fn new_tab(&mut self) {
        self.next_id += 1;
        let title = if self.sessions.is_empty() {
            std::path::Path::new(&self.cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            String::new()
        };
        let mut s = Session::new(self.next_id, title);
        s.profile = self.default_profile.clone();
        if !self.profile_forced && let Some(p) = project_profile(&self.cwd) {
            s.profile = p;
        }
        s.cache_ttl = self.cache_ttl;
        // Shown until claude reports the real mode, so a new tab does not claim "manual" by mistake.
        let flag = self.launch.args.iter().position(|a| a == "--permission-mode").and_then(|i| self.launch.args.get(i + 1));
        s.mode = flag.cloned().unwrap_or_else(|| permissions::default_mode(&self.cwd));
        self.leave_tab();
        self.sessions.push(s);
        self.active = self.sessions.len() - 1;
        self.load_draft();
    }

    /// Called just before another tab takes the front. Leaving counts as use, so its idle time restarts here
    /// (otherwise a tab idle for minutes is stopped the moment you look away); its `/tasks` list is its own.
    fn leave_tab(&mut self) {
        if let Some(s) = self.sessions.get_mut(self.active) {
            s.last_active = Instant::now();
        }
        self.lanes = None;
        self.ctx_ui = None;
        self.queue_ui = None;
        self.sched_ui = None;
        self.sched_at = None;
        self.search = None;
        self.save_draft();
    }

    /// The message box belongs to the tab in front: leaving a tab keeps what was typed in it.
    fn save_draft(&mut self) {
        self.reset_undo = true;
        if let Some(s) = self.sessions.get_mut(self.active) {
            s.draft = (std::mem::take(&mut self.input), self.cursor);
        }
    }

    fn load_draft(&mut self) {
        let s = &mut self.sessions[self.active];
        (self.input, self.cursor) = std::mem::take(&mut s.draft);
        self.cursor = self.cursor.min(self.input.chars().count());
    }

    fn goto(&mut self, i: usize) {
        if i != self.active {
            self.leave_tab();
            self.active = i;
            self.load_draft();
            self.files_at = None; // the `@` index belongs to the tab's directory
        }
    }

    /// Closing a tab that is mid-turn throws the turn away: ask once, by pressing the same thing again.
    fn close_tab_asking(&mut self) {
        let s = &self.sessions[self.active];
        let busy = s.busy || !s.pending.is_empty();
        if busy && !self.close_armed.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            self.close_armed = Some(Instant::now());
            return self.flash("this tab is still working: close it again to stop the turn and close");
        }
        self.close_armed = None;
        self.close_tab();
    }

    fn close_tab(&mut self) {
        self.sessions.remove(self.active);
        if self.sessions.is_empty() {
            self.quit = true;
            self.new_tab(); // keep `active` valid until the loop exits
        }
        self.active = self.active.min(self.sessions.len() - 1);
        self.load_draft(); // the closed tab's own draft goes with it
    }

    fn switch(&mut self, delta: isize) {
        let n = self.sessions.len() as isize;
        self.goto(((self.active as isize + delta).rem_euclid(n)) as usize);
    }

    fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Backend(sid, pid, msg) => {
                let at = self.sessions.iter().position(|s| s.id == sid);
                let before = at.map(|i| (self.sessions[i].busy, self.sessions[i].pending.len()));
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.on_backend(pid, msg);
                    if std::mem::take(&mut s.cmds_new) && s.commands.len() != self.known_commands.len() {
                        self.known_commands = s.commands.clone();
                        save_commands(&self.known_commands);
                    }
                    if std::mem::take(&mut s.models_new) && !s.models.is_empty() {
                        self.known_models = s.models.clone();
                        if let Some(p) = cache_path("models.json") {
                            write_models(&p, &self.known_models);
                        }
                    }
                }
                // A tab you are not looking at finished a turn or wants an answer.
                if let (Some(i), Some((was_busy, was_pending))) = (at.filter(|i| *i != self.active), before) {
                    let s = &self.sessions[i];
                    let name = if s.title.is_empty() { format!("tab {}", i + 1) } else { s.title.clone() };
                    if s.pending.len() > was_pending {
                        self.notify(&format!("{name} needs you"));
                    } else if was_busy && !s.busy {
                        self.notify(&format!("{name} finished"));
                    }
                }
                self.drain_queues();
            }
            AppEvent::Pane(id, msg) => self.on_pane(id, msg),
            AppEvent::Mem(mem, own) => self.on_mem(mem, own),
            AppEvent::Theme(light) => {
                theme::set_light(light);
                self.restyle();
            }
            AppEvent::Files(cwd, list) => {
                if cwd == self.cwd() {
                    self.files = list;
                    self.file_key = None; // re-rank what is typed against the new list
                    self.refresh_files();
                }
            }
            AppEvent::Rewound(sid, said) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.rewinding = false;
                    s.note(said.unwrap_or_else(|e| format!("files not restored: {e}")));
                }
            }
            AppEvent::Shell(sid, id, cmd, out) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    match out {
                        Ok(o) => s.shell_done(&id, &cmd, &String::from_utf8_lossy(&o.stdout), &String::from_utf8_lossy(&o.stderr), o.status.success()),
                        Err(e) => s.shell_done(&id, &cmd, "", &e.to_string(), false),
                    }
                }
            }
            AppEvent::Clipboard(sid, png, miss) => match png {
                // A picture lands in the box it was pasted into, or nowhere.
                Some(png) if self.sessions[self.active].id == sid => {
                    self.clip_n += 1;
                    self.attach_bytes(format!("clipboard-{}.png", self.clip_n), png, "image/png");
                }
                Some(_) => self.flash("pasted image dropped: the tab changed while it was read"),
                None => self.flash(miss),
            },
            AppEvent::Stopped(res, then) => match res {
                Ok(msg) => {
                    self.flash(msg);
                    if let Some((id, title, cwd)) = then {
                        self.open_session(id, title, cwd);
                    }
                }
                Err(e) => self.flash(format!("could not stop it: {e}")),
            },
            AppEvent::Status(sid, run, lines) => {
                let pad = self.statusline.as_ref().map_or(0, |c| c.padding);
                // A cancelled run's result arrives late; only the latest run's counts.
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid && s.status_gen == run) {
                    s.status_running = false;
                    if let Some(lines) = lines {
                        s.status_lines = lines
                            .iter()
                            .map(|l| {
                                let mut line = statusline::parse_line(l);
                                if pad > 0 {
                                    line.spans.insert(0, ratatui::text::Span::raw(" ".repeat(pad)));
                                }
                                line
                            })
                            .collect();
                    }
                }
            }
            AppEvent::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                self.last_input = Instant::now();
                // Any key drops the selection, except ctrl+y, which copies it.
                if !(k.code == KeyCode::Char('y') && k.modifiers.contains(KeyModifiers::CONTROL)) {
                    self.drag = None;
                }
                self.on_key(k)
            }
            AppEvent::Term(Event::Paste(p)) => {
                self.last_input = Instant::now();
                self.on_paste(p)
            }
            AppEvent::Term(Event::Mouse(m)) => {
                let p = (m.column, m.row);
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.last_input = Instant::now();
                        self.drag = self.hits.body.contains(p.into()).then_some((p, p));
                        self.dragging = self.drag.is_some();
                        return self.click(m.column, m.row);
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some((a, _)) = self.drag.filter(|_| self.dragging) {
                            self.drag = Some((a, p));
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if self.dragging {
                            self.dragging = false;
                            self.drag = self.drag.map(|(a, _)| (a, p)).filter(|s| s.0 != s.1);
                            if self.autocopy {
                                self.copy_selection();
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        self.drag = None; // the cells it marked have moved
                        self.s().scroll += 3
                    }
                    MouseEventKind::ScrollDown => {
                        self.drag = None;
                        let s = self.s();
                        s.scroll = s.scroll.saturating_sub(3)
                    }
                    _ => {}
                }
            }
            AppEvent::Quit => self.quit = true,
            AppEvent::Term(_) => {}
        }
    }

    /// Start a status line refresh for the active tab if something it shows has changed. This follows
    /// Claude Code's documented triggers: first run, a new reply, `/compact`, a permission-mode change, the
    /// `refreshInterval` timer, a limit window or the prompt cache reaching its reset/expiry time. Updates are
    /// spaced 300 ms apart, and a new one cancels a run still in flight. The timer only runs while the user is
    /// around, so an idle claude++ starts nothing.
    /// Choose which status line is shown and resolve it. Returns a sentence to tell the user when the
    /// choice could not be honoured as asked (nothing configured, or a project command not yet trusted).
    fn apply_statusline_mode(&mut self, mode: statusline::Mode) -> Option<String> {
        let mut note = None;
        self.statusline = match &mode {
            statusline::Mode::Builtin => None,
            statusline::Mode::Command(c) => Some(statusline::Config { command: c.clone(), interval: None, padding: 0, source: statusline::Source::Flag }),
            statusline::Mode::Claude => {
                let found = statusline::lookup(std::path::Path::new(self.cwd()), &self.claude_paths);
                if let Some((source, cmd)) = found.untrusted {
                    note = Some(format!("{source:?} status line `{cmd}` is not run: Claude Code has not trusted this folder yet (start `claude` here once and accept the trust prompt)"));
                } else if found.found.is_none() {
                    note = Some("no `statusLine` in your Claude Code settings".into());
                }
                found.found
            }
        };
        self.statusline_on = self.statusline.is_some();
        self.statusline_mode = mode;
        self.sessions.iter_mut().for_each(|s| s.status_dirty = true);
        note
    }

    fn pump_statusline(&mut self) {
        let Some(cfg) = self.statusline.clone().filter(|_| self.statusline_on) else { return };
        let id = self.sessions[self.active].id;
        if self.status_for != id {
            self.status_for = id;
            self.sessions[self.active].status_dirty = true;
        }
        let around = self.last_input.elapsed() < Duration::from_secs(600);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let cwd = self.cwd().to_string();
        let s = &mut self.sessions[self.active];
        let timed = s.status_next.is_some_and(|t| now >= t);
        let periodic = around && cfg.interval.is_some_and(|i| s.status_ran.is_some_and(|t| t.elapsed() >= i));
        let due = s.status_dirty || s.status_ran.is_none() || periodic || timed;
        if !due || s.status_ran.is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
            return;
        }
        s.status_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        s.status_cancel = Default::default();
        s.status_gen += 1;
        let (run, cancel, sid) = (s.status_gen, s.status_cancel.clone(), s.id);
        let mut input = s.status_json(&cwd);
        s.status_running = true;
        s.status_dirty = false;
        s.status_ran = Some(Instant::now());
        let size = terminal::size().unwrap_or((80, 24));
        let tx = self.tx.clone();
        thread::spawn(move || {
            statusline::enrich(&mut input, &cwd);
            let lines = statusline::run(&cfg.command, &input.to_string(), size, &cwd, &cancel);
            let _ = tx.send(AppEvent::Status(sid, run, lines));
        });
    }

    fn tick(&mut self) {
        // An editor that exited but left something holding its terminal open never reports the end itself.
        if self.pane.as_mut().is_some_and(|p| p.exited()) {
            self.finish_pane();
        }
        self.keep_editor_open();
        self.drain_queues(); // scheduled messages come due with the clock
        self.pump_statusline();
        for s in &mut self.sessions {
            s.poll_mcp();
        }
        self.opt_poll();
        if self.dock_at.elapsed() > Duration::from_secs(5) && self.dock.is_some() {
            let (here, open) = (self.cwd().to_string(), self.open_states());
            if let Some(d) = self.dock.as_mut() {
                d.refresh(&here, open);
            }
            self.dock_at = Instant::now();
        }
        if self.flash.is_some() && self.flash_at.elapsed() > Duration::from_secs(4) {
            self.flash = None;
        }
        let ttl = self.cache_ttl;
        for s in &mut self.sessions {
            if s.cache_left(ttl).is_some_and(|l| l.is_zero()) && !s.cold_noted {
                s.cold_noted = true;
                let k = s.ctx_tokens / 1000;
                s.note(format!("prompt cache expired after {}m idle: your next message re-caches ~{k}k tokens (a fresh /new tab would not)", ttl.as_secs() / 60));
            }
        }
        // Sampling spawns `ps` and `footprint`: every 5 s only while a memory budget needs it.
        if self.last_sample.elapsed() < Duration::from_secs(if self.mem_budget_kb > 0 { 5 } else { 15 }) {
            return;
        }
        self.last_sample = Instant::now();
        if !self.idle_suspend.is_zero() {
            for (i, s) in self.sessions.iter_mut().enumerate() {
                // The tab in front too, once you have been away five times as long: a resting claude++ is ~4 MB,
                // and typing restarts the backend (`warm`) while you compose.
                let away = i != self.active || self.last_input.elapsed() > self.idle_suspend * 5;
                if away && s.last_active.elapsed() > self.idle_suspend && s.suspend() {
                    s.note("idle: claude process stopped to free memory; it resumes on your next message".into());
                }
            }
        }
        // `ps` and `footprint` take 20–90 ms: measured off the UI thread, applied when they report.
        let roots: Vec<u32> = self.sessions.iter().filter_map(|s| s.backend.as_ref().map(|b| b.pid)).collect();
        let sample = move || {
            let me = std::process::id();
            // Our own process only: its tree would include the backends, which are counted separately.
            let own = footprint_kb(&[me]).first().or(rss_kb(&[me]).first()).map_or(0, |r| r.1);
            (tree_mem_kb(&roots), own)
        };
        if cfg!(test) {
            let (mem, own) = sample(); // tests read their event channel themselves
            return self.on_mem(mem, own);
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (mem, own) = sample();
            let _ = tx.send(AppEvent::Mem(mem, own));
        });
    }

    fn on_mem(&mut self, mem: Vec<(u32, u64)>, own: u64) {
        for s in &mut self.sessions {
            s.mem_kb = s.backend.as_ref().and_then(|b| mem.iter().find(|r| r.0 == b.pid)).map_or(0, |r| r.1);
        }
        self.ui_mem_kb = own;
        // A full-profile tab carries MCP servers and hooks: say once what `lean` would save.
        for s in &mut self.sessions {
            if s.profile == "full" && s.mem_kb > HEAVY_TAB_KB && !std::mem::replace(&mut s.lean_hinted, true) {
                let mb = s.mem_kb / 1024;
                s.note(format!("this tab's claude process uses {mb} MB; `/profile lean` runs a single process of about 220 MB (core tools only, no MCP servers or user hooks). `/new lean` opens one."));
            }
        }
        self.enforce_budget();
    }

    /// Over budget: stop the least-recently-used tab that is safe to stop, one per sample, so the
    /// next sample sees the real effect before another goes.
    fn enforce_budget(&mut self) {
        let total: u64 = self.sessions.iter().map(|s| s.mem_kb).sum::<u64>() + self.ui_mem_kb;
        if self.mem_budget_kb == 0 || total <= self.mem_budget_kb {
            return;
        }
        let active = self.active;
        let victim = (0..self.sessions.len())
            .filter(|&i| i != active && self.sessions[i].can_suspend())
            .min_by_key(|&i| self.sessions[i].last_active);
        match victim {
            Some(i) => {
                self.sessions[i].suspend();
                self.sessions[i].note("memory budget: claude process stopped to free memory; it resumes on your next message".into());
            }
            None if self.budget_warned.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) => {
                self.budget_warned = Some(Instant::now());
                self.flash(format!("over memory budget ({} MB) and no tab is safe to stop", total / 1024));
            }
            None => {}
        }
    }

    fn on_paste(&mut self, p: String) {
        if self.pane_has_keyboard() {
            self.pane_typed = true;
            if self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send) {
                self.warm_active();
            }
            return self.pane.as_mut().map_or((), |e| e.paste(&p));
        }
        if self.overlay_open() {
            return;
        }
        // Input mode editor: the paste is what the editor opens with (keys sent before it is ready could be commands).
        if self.editor_mode && !self.pane_failed && self.pane.is_none() && self.sessions[self.active].pending.is_empty() {
            let text = format!("{}{}", std::mem::take(&mut self.input), p.replace("\r\n", "\n").replace('\r', "\n"));
            match self.open_pane(pane::Use::Send, &text) {
                Ok(()) => return self.cursor = 0,
                Err(e) => {
                    self.input = text;
                    self.cursor = self.input.chars().count();
                    return self.flash(format!("could not start the editor ({e}): pasted into the box"));
                }
            }
        }
        // A question takes pasted text as its typed answer; a permission prompt only as the reason on its
        // "No" row (a pasted "y" must never approve anything).
        if let Some(pending) = self.sessions[self.active].pending.front_mut() {
            if pending.is_question() {
                let qs = pending.questions().to_vec();
                pending.ui.question_paste(&qs, &p);
            } else {
                pending.ui.paste_feedback(&p, pending.suggestions.as_array().is_some_and(|a| !a.is_empty()));
            }
            return;
        }
        if p.is_empty() {
            // Terminals send an empty paste when the clipboard holds an image and no text.
            self.paste_image("nothing to paste");
            return;
        }
        let p = p.replace("\r\n", "\n").replace('\r', "\n");
        // Dragging an image into the terminal pastes its path: attach it instead.
        let path = p.trim().trim_matches(|c| c == '\'' || c == '"').replace("\\ ", " ");
        if !path.contains('\n') && image::media_type(&path).is_some() && std::path::Path::new(&path).is_file() {
            self.attach(&path);
            return;
        }
        self.insert(&p);
        self.warm();
        self.refresh_files();
    }

    /// Length of an `[Image #n]` token ending (`back`) or starting at the cursor, so it is deleted whole.
    fn token_len(&self, back: bool) -> Option<usize> {
        let chars: Vec<char> = self.input.chars().collect();
        if back { image::token_ending_at(&chars, self.cursor) } else { image::token_starting_at(&chars, self.cursor) }
    }

    /// Read the clipboard's image on a thread (osascript takes ~100–200 ms); it is attached when it arrives.
    fn paste_image(&mut self, miss: &'static str) {
        let (sid, tx) = (self.sessions[self.active].id, self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(AppEvent::Clipboard(sid, image::clipboard_png(), miss));
        });
    }

    fn attach(&mut self, path: &str) {
        let Some(mt) = image::media_type(path) else {
            return self.flash("only png/jpeg/gif/webp images can be attached");
        };
        match std::fs::read(path) {
            Ok(b) => {
                let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy().into_owned();
                self.attach_bytes(name, b, mt);
            }
            Err(e) => self.flash(format!("{path}: {e}")),
        }
    }

    /// Queue an image for the next message. Over the API's 5 MB limit it is re-encoded smaller first.
    fn attach_bytes(&mut self, name: String, bytes: Vec<u8>, media: &str) {
        const LIMIT: usize = 5 * 1024 * 1024;
        let (bytes, media) = if bytes.len() > LIMIT {
            match image::shrink(&bytes).filter(|b| b.len() <= LIMIT) {
                Some(b) => (b, "image/png"),
                None => return self.flash("image is over the 5 MB API limit and could not be shrunk"),
            }
        } else {
            (bytes, media)
        };
        let block = json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": image::base64(&bytes)}});
        let png = if image::enabled() { image::prepare(&bytes, media) } else { None };
        let width = ratatui::crossterm::terminal::size().map_or(80, |s| s.0 as u32);
        let preview = png.as_deref().and_then(|p| image::upload(p, width.saturating_sub(6).min(36), 5));
        let n = self.s().attachments.iter().map(|a| a.n).max().unwrap_or(0) + 1;
        self.s().attachments.push(session::Attachment { n, name: name.clone(), block, png, preview });
        // The picture lives in the text as `[Image #n]`, on its own line, wherever the cursor was.
        let chars: Vec<char> = self.input.chars().collect();
        let before = self.cursor > 0 && chars.get(self.cursor - 1) != Some(&'\n');
        let after = chars.get(self.cursor) != Some(&'\n');
        let token = format!("{}{}{}", if before { "\n" } else { "" }, image::token(n), if after { "\n" } else { "" });
        self.insert(&token);
        self.flash(format!("attached {name} as {}", image::token(n)));
    }

    fn insert(&mut self, t: &str) {
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, t);
        self.cursor += t.chars().count();
    }

    /// Start of the word before the cursor (whitespace, newlines included, separates words).
    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// End of the word after the cursor.
    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Remove the characters between two positions; the cursor ends at the lower one.
    fn delete_range(&mut self, from: usize, to: usize) {
        let (a, b) = (self.byte_at(from.min(to)), self.byte_at(from.max(to)));
        self.input.replace_range(a..b, "");
        self.cursor = from.min(to);
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input.char_indices().nth(char_idx).map_or(self.input.len(), |(b, _)| b)
    }

    fn set_input(&mut self, t: String) {
        self.cursor = t.chars().count();
        self.input = t;
    }

    /// With the editor open, which keys are claude++'s (translated to the key the box expects) and which are the editor's.
    /// Claimed: chords no editor needs (alt+1..9 o q p, pgup/pgdn, shift+↑↓, ctrl+home/end, shift+tab, ctrl+t o l);
    /// alt+f r d b n and alt+←/→ stand for ctrl+f r d b n and ctrl+p/n, which the editor needs for itself;
    /// and, while nothing has been typed yet, ↑ ↓ (history), ← (session list) and Esc (interrupt, rewind).
    fn app_key(&self, k: KeyEvent) -> Option<KeyEvent> {
        use KeyCode::*;
        let m = k.modifiers;
        let (ctrl, alt, shift) = (m.contains(KeyModifiers::CONTROL), m.contains(KeyModifiers::ALT), m.contains(KeyModifiers::SHIFT));
        let as_ctrl = |c: char| Some(KeyEvent::new(Char(c), KeyModifiers::CONTROL));
        match k.code {
            Char(c) if alt && !ctrl && matches!(c, 'f' | 'r' | 'd' | 'b' | 'n') => as_ctrl(c),
            Left if alt => as_ctrl('p'),
            Right if alt => as_ctrl('n'),
            Char('1'..='9' | 'o' | 'q' | 'p') if alt && !ctrl => Some(k),
            Char('t' | 'o' | 'l') if ctrl && !alt => Some(k),
            PageUp | PageDown | BackTab => Some(k),
            Up | Down if shift => Some(k),
            Home | End if ctrl => Some(k),
            Up | Down | Left | Esc if !self.pane_typed && m.is_empty() => Some(k),
            _ => None,
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.pane_has_keyboard() {
            let send = self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send);
            let Some(app) = send.then(|| self.app_key(k)).flatten() else { return self.pane_key(k) };
            let before = self.hist_pos;
            self.on_key_app(app);
            // History put a message in the box: that is the editor's text now.
            if self.hist_pos != before && self.pane.is_some() {
                self.pane = None;
                self.pane_closed();
                self.keep_editor_open();
            }
            return;
        }
        self.on_key_app(k)
    }

    fn on_key_app(&mut self, k: KeyEvent) {
        let box_is_free = !self.overlay_open() && self.sessions[self.active].pending.is_empty();
        let (ctrl, alt, shift) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT), k.modifiers.contains(KeyModifiers::SHIFT));
        // Input mode editor, when the editor could not be opened up front: typing retries it.
        if self.editor_mode && !self.pane_failed && self.pane.is_none() && box_is_free && !ctrl && !alt && matches!(k.code, KeyCode::Char(_)) {
            let text = std::mem::take(&mut self.input);
            match self.open_pane(pane::Use::Send, &text) {
                Ok(()) => {
                    self.cursor = 0;
                    return self.pane_key(k);
                }
                Err(e) => {
                    self.input = text;
                    self.flash(format!("could not start the editor ({e}): typing in the box"));
                }
            }
        }
        // ctrl+z undoes; ctrl+shift+z redoes, and so does alt+z for terminals that cannot tell shift apart.
        // In vim's normal mode `u` and ctrl+r do the same.
        let in_normal = self.vim.on && self.vim.mode == vim::Mode::Normal && !self.vim.pending() && !alt;
        let redo_asked = match k.code {
            KeyCode::Char('z' | 'Z') if box_is_free && (ctrl || alt) => Some(shift || alt),
            KeyCode::Char('_' | '7') if box_is_free && ctrl => Some(false), // Claude Code's undo key
            KeyCode::Char('u') if box_is_free && in_normal && !ctrl => Some(false),
            KeyCode::Char('r') if box_is_free && in_normal && ctrl => Some(true),
            _ => None,
        };
        if let Some(redo) = redo_asked {
            let (from, to, none) = if redo { (&mut self.redo, &mut self.undo, "nothing to redo") } else { (&mut self.undo, &mut self.redo, "nothing to undo") };
            match from.pop() {
                Some(state) => {
                    to.push((std::mem::replace(&mut self.input, state.0), self.cursor));
                    self.cursor = state.1;
                }
                None => self.flash(none),
            }
            return self.vim_settle();
        }
        let before = (self.input.clone(), self.cursor);
        self.reset_undo = false;
        self.handle_key(k);
        if std::mem::take(&mut self.reset_undo) {
            self.undo.clear(); // sent, or a different tab's box: nothing earlier belongs to this one
            self.redo.clear();
        } else if self.input != before.0 {
            self.redo.clear();
            // A burst of typing is one undo step; any other edit, or a pause, starts a new one.
            let typing = self.input.chars().count() == before.0.chars().count() + 1 && self.cursor == before.1 + 1;
            if !typing || self.undo.is_empty() || self.last_edit.elapsed() > Duration::from_millis(800) {
                self.undo.push(before.clone());
                self.undo.drain(..self.undo.len().saturating_sub(100));
            }
            self.last_edit = Instant::now();
        }
        if self.input != before.0 {
            self.sel = 0;
            self.drop_orphan_images("");
        }
        self.refresh_files();
        self.vim_shape();
    }

    /// Free attachments whose `[Image #n]` token is in neither the input, `also`, nor anything ctrl+z can bring back.
    fn drop_orphan_images(&mut self, also: &str) {
        if self.sessions[self.active].attachments.is_empty() {
            return;
        }
        let mut present = image::tokens_in(&self.input);
        present.extend(image::tokens_in(also));
        present.extend(self.undo.iter().chain(&self.redo).flat_map(|(t, _)| image::tokens_in(t)));
        self.s().attachments.retain(|a| {
            let keep = present.contains(&a.n);
            if !keep {
                a.preview.iter().for_each(|p| image::delete(p.id));
            }
            keep
        });
    }

    /// The `@word` being typed at the cursor: (char index of the `@`, text after it).
    fn at_token(&self) -> Option<(usize, String)> {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        (chars.get(start) == Some(&'@') && start < self.cursor).then(|| (start, chars[start + 1..self.cursor].iter().collect()))
    }

    /// Keep the `@` popup's rows in step with the word under the cursor.
    fn refresh_files(&mut self) {
        let tok = if self.overlay_open() { None } else { self.at_token() };
        let Some((_, q)) = tok else {
            self.file_rows.clear();
            self.file_key = None;
            return;
        };
        if self.file_key.as_deref() == Some(q.as_str()) {
            return;
        }
        if self.file_key.is_none() && self.files_at.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            // Listing walks the tree outside a git repo (over a second in a home folder): built off the
            // UI thread, the popup ranks the previous list until it arrives.
            self.files_at = Some(Instant::now());
            let (cwd, tx) = (self.cwd().to_string(), self.tx.clone());
            if cfg!(test) {
                self.files = files::list(&cwd); // tests read their event channel themselves
            } else {
                std::thread::spawn(move || {
                    let list = files::list(&cwd);
                    let _ = tx.send(AppEvent::Files(cwd, list));
                });
            }
        }
        self.file_rows = files::rank(&self.files, &q, 30)
            .into_iter()
            .map(|name| {
                let desc = if name.ends_with('/') { "folder" } else if image::media_type(&name).is_some() { "image" } else { "" };
                Completion { prefix: '@', desc: desc.into(), name, hint: String::new() }
            })
            .collect();
        self.file_key = Some(q);
    }

    /// Rows for the popup: `/commands` when the whole input is one slash word, else `@files`.
    pub fn completions(&self) -> Vec<Completion> {
        if self.comp_off.as_deref() == Some(self.input.as_str()) || self.sessions[self.active].pending.front().is_some() {
            return vec![];
        }
        let Some(q) = self.input.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)) else {
            return self.file_rows.clone();
        };
        let theirs = &self.sessions[self.active].commands;
        let theirs = if theirs.is_empty() { &self.known_commands } else { theirs };
        let q = q.to_lowercase();
        // Sorted by (name only contains the query, not one of ours, name): prefix hits first, ours before Claude Code's.
        let mut rows: Vec<(bool, bool, Completion)> = LOCAL_COMMANDS
            .iter()
            .map(|(n, h, d)| (true, Completion { prefix: '/', name: n.to_string(), hint: h.to_string(), desc: theme::keys(d) }))
            .chain(
                theirs
                    .iter()
                    .filter(|c| !LOCAL_COMMANDS.iter().any(|l| l.0 == c.name))
                    .map(|c| (false, Completion { prefix: '/', name: c.name.clone(), hint: c.hint.clone(), desc: c.desc.clone() })),
            )
            .filter_map(|(ours, c)| {
                let n = c.name.to_lowercase();
                if n.starts_with(&q) {
                    Some((false, !ours, c))
                } else {
                    n.contains(&q).then_some((true, !ours, c))
                }
            })
            .collect();
        rows.sort_by(|a, b| (a.0, a.1, &a.2.name).cmp(&(b.0, b.1, &b.2.name)));
        rows.into_iter().map(|r| r.2).collect()
    }

    fn accept_completion(&mut self) {
        let rows = self.completions();
        let Some(c) = rows.get(self.sel.min(rows.len().saturating_sub(1))) else { return };
        if c.prefix == '/' {
            return self.set_input(format!("/{} ", c.name));
        }
        // A folder keeps the popup open for the next level; a file ends the word.
        let Some((start, _)) = self.at_token() else { return };
        let insert = format!("@{}{}", c.name, if c.name.ends_with('/') { "" } else { " " });
        let (a, b) = (self.byte_at(start), self.byte_at(self.cursor));
        self.input.replace_range(a..b, &insert);
        self.cursor = start + insert.chars().count();
    }

    /// Wheel scrolling needs the terminal to report clicks and wheel (modes 1000/1006, not motion); the cost
    /// is that selecting text takes Shift (Option in some terminals) while it is on.
    /// Tell the user something happened in a tab they are not looking at: a bell, or a desktop notification
    /// (OSC 9: iTerm2, Ghostty, WezTerm...), as configured.
    fn notify(&mut self, what: &str) {
        let seq = match self.notify.as_str() {
            "bell" => "\x07".to_string(),
            "desktop" => format!("\x1b]9;claude++: {}\x07", what.replace(['\x07', '\x1b'], "")),
            _ => return,
        };
        #[cfg(test)]
        self.sent_notices.push(seq.clone());
        if self.tty {
            let _ = write!(io::stdout(), "{seq}");
            let _ = io::stdout().flush();
        }
    }

    /// `/copy` and ctrl+y: the last answer, or with `code` the last fenced code block in it.
    /// `/export [file]`: the conversation as plain text, your messages marked `>` and tool calls `●`.
    fn export(&mut self, arg: &str) {
        let mut out = String::new();
        for i in &self.sessions[self.active].items {
            let block = match &i.entry {
                Entry::User(_) | Entry::Rich(_) => i.entry.text().lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n"),
                Entry::Assistant { text, .. } => text.trim_end().to_string(),
                Entry::Tool { name, input, result, nested, .. } if !nested && name != "Thinking" => {
                    let res = result.as_ref().map_or(String::new(), |r| {
                        let mut t: String = r.0.trim_end().chars().take(2000).collect();
                        if t.len() < r.0.trim_end().len() {
                            t.push_str("\n…");
                        }
                        t.lines().map(|l| format!("\n  ⎿ {l}")).collect()
                    });
                    let mut input = input.clone();
                    if let Some(o) = input.as_object_mut() {
                        o.retain(|k, _| !k.starts_with('_')); // our own bookkeeping (patches, agent steps)
                    }
                    format!("● {name}({input}){res}")
                }
                _ => continue,
            };
            out.push_str(&block);
            out.push_str("\n\n");
        }
        if out.is_empty() {
            return self.flash("nothing to export yet");
        }
        let name = if arg.is_empty() {
            let stamp = std::process::Command::new("date").arg("+%Y-%m-%d-%H%M%S").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            format!("conversation-{stamp}.txt")
        } else {
            arg.to_string()
        };
        let path = std::path::Path::new(self.cwd()).join(name);
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path);
        match file.and_then(|mut f| f.write_all(out.as_bytes())) {
            Ok(()) => self.flash(format!("exported to {}", path.display())),
            Err(e) => self.flash(format!("export failed: {e}")),
        }
    }

    fn copy_last(&mut self, what: &str) {
        let last = self.sessions[self.active].items.iter().rev().find_map(|i| match &i.entry {
            Entry::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        let Some(text) = last else { return self.flash("nothing to copy yet") };
        let (text, label) = match what {
            "" => (text, "answer"),
            "code" => match last_code_block(&text) {
                Some(code) => (code, "code block"),
                None => return self.flash("the last answer has no code block"),
            },
            _ => return self.flash("usage: /copy [code]"),
        };
        let n = text.chars().count();
        self.copy_text(&text);
        self.flash(format!("copied the last {label} ({n} chars)"));
    }

    /// Copy what the mouse selected, if anything; false when there was nothing.
    fn copy_selection(&mut self) -> bool {
        let Some((a, b)) = self.drag else { return false };
        let text = ui::selection_text(&self.snap, self.hits.body, a, b);
        if text.is_empty() {
            return false;
        }
        self.copy_text(&text);
        self.flash("copied");
        true
    }

    /// Clipboard via the OSC 52 escape (works over ssh in terminals that allow it) and, where it exists,
    /// `pbcopy`.
    fn copy_text(&mut self, text: &str) {
        #[cfg(test)]
        self.sent_notices.push(format!("copy:{text}"));
        if self.tty {
            let _ = write!(io::stdout(), "\x1b]52;c;{}\x07", image::base64(text.as_bytes()));
            let _ = io::stdout().flush();
        }
        if cfg!(target_os = "macos") && self.tty
            && let Ok(mut c) = std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn()
        {
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(text.as_bytes());
            }
            thread::spawn(move || drop(c.wait()));
        }
    }

    /// A left click: a tab switches to it; a row of a list moves the cursor there, and a click on the row
    /// the cursor is already on does what Enter does. Done as the keys, so every panel keeps its own rules.
    fn click(&mut self, col: u16, row: u16) {
        let hits = self.hits.clone();
        if row == hits.tab_y
            && let Some(i) = hits.tabs.iter().position(|(a, b)| (*a..*b).contains(&col))
        {
            self.dock_peek = false;
            return self.goto(i);
        }
        // A permission prompt or question is answered with the keys; a stray click must not pick for you.
        if !self.sessions[self.active].pending.is_empty() {
            return;
        }
        if hits.queue.is_some_and(|r| row >= r.y && row < r.bottom()) {
            return self.queue_ui = Some(Default::default());
        }
        let (hit, in_dock) = match (hits.dock, hits.list) {
            (Some((r, h)), _) if !self.overlay_open() && col >= r.x && col < r.right() && row >= r.y && row < r.bottom() => (Some(h), true),
            (_, l) => (l, false),
        };
        let Some(target) = hit.and_then(|h| h.row_at(col, row)) else {
            // Not on a list: a click on a tool call in the conversation folds or unfolds it.
            if hit.is_none() && let Some(&(_, _, i)) = hits.calls.iter().find(|c| (c.0..c.1).contains(&row)) {
                self.s().flip_call(Some(i));
            }
            return;
        };
        let h = hit.unwrap();
        if in_dock {
            self.dock_focus = true;
        }
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let (code, n) = if target >= h.cursor { (KeyCode::Down, target - h.cursor) } else { (KeyCode::Up, h.cursor - target) };
        (0..n).for_each(|_| self.handle_key(key(code)));
        if n == 0 || h.input_focus {
            self.handle_key(key(if h.input_focus { KeyCode::Tab } else { KeyCode::Enter }));
        }
    }

    fn set_mouse(&mut self, on: bool) {
        self.mouse = on;
        if self.tty {
            let _ = write!(io::stdout(), "\x1b[?1002{0}\x1b[?1000{0}\x1b[?1006{0}", if on { 'h' } else { 'l' });
            let _ = io::stdout().flush();
        }
    }

    #[cfg(test)]
    fn profile_flag_error(&self, p: &str) -> bool {
        profile_args(p).is_err()
    }

    /// Every open tab that has a stored conversation, with what it is doing now.
    pub fn open_states(&self) -> Vec<(String, jobs::State)> {
        self.sessions
            .iter()
            .filter_map(|s| {
                let st = if !s.pending.is_empty() { jobs::State::Blocked } else if s.busy { jobs::State::Working } else { jobs::State::Done };
                s.session_id.clone().map(|id| (id, st))
            })
            .collect()
    }

    /// The side panel follows the cursor: an open conversation is switched to at once; a stored one is
    /// previewed in the main area (`dock_peek`) until Enter opens it in a new tab.
    fn dock_follow(&mut self) {
        let Some((id, tab)) = self.dock.as_ref().and_then(|d| d.selected()) else {
            self.dock_peek = false; // on a heading: nothing to follow
            return;
        };
        self.dock_peek = !tab;
        if let Some(i) = self.sessions.iter().position(|s| s.session_id.as_deref() == Some(id.as_str())) {
            self.goto(i);
        }
    }

    fn show_dock(&mut self, focus: bool) {
        if self.dock.is_none() {
            let open = self.open_states();
            let mut d = explorer::Explorer::new(self.cwd(), open);
            d.set_folds(self.list_prefs.1.clone());
            self.dock = Some(d);
            self.dock_at = Instant::now();
        }
        self.dock_focus = focus;
    }

    /// /sessions: the docked list when configured (toggle), else the full-screen one.
    fn toggle_sessions(&mut self) {
        if !self.dock_mode {
            self.open_explorer();
        } else if self.dock.take().is_some() {
            self.dock_focus = false;
        } else {
            self.show_dock(true);
        }
    }

    fn dock_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Left => self.open_explorer(),
            KeyCode::Esc => (self.dock_focus, self.dock_peek) = (false, false),
            _ => {
                if let Some(o) = self.dock.as_mut().map(|d| d.key(k)) {
                    // → is "done browsing": the conversation in view stays, the panel goes.
                    let leave = matches!(o, explorer::Outcome::OpenHere(..) | explorer::Outcome::Close);
                    let opened = matches!(o, explorer::Outcome::Open(..));
                    self.explorer_outcome(o);
                    if leave {
                        (self.dock, self.dock_focus, self.dock_peek) = (None, false, false);
                    } else if opened {
                        (self.dock_focus, self.dock_peek) = (false, false); // typing now goes to that conversation
                    } else {
                        self.dock_follow();
                    }
                }
            }
        }
    }

    /// What a list row asked for (open, fork, stop...), from the full-screen list or the dock.
    fn explorer_outcome(&mut self, o: explorer::Outcome) {
        use explorer::Outcome::*;
        match o {
            Open(id, title, cwd) => self.open_session(id, title, cwd),
            OpenHere(id, title, cwd) => self.open_here(id, title, cwd),
            Fork(id, title, cwd) => {
                self.open_in_tab(id, title, cwd);
                self.s().fork = true;
                self.s().note("forked: this is a copy; the original is untouched".into());
            }
            Stop(short, then) => {
                self.flash(format!("stopping {short}…"));
                let (bin, tx) = (self.launch.bin.clone(), self.tx.clone());
                thread::spawn(move || {
                    let r = std::process::Command::new(bin).args(["stop", &short]).output();
                    let res = match r {
                        Ok(o) if o.status.success() => Ok(format!("stopped {short}")),
                        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(AppEvent::Stopped(res, then));
                });
            }
            Refused(why) => self.flash(why),
            Stay | Close => {}
        }
    }

    fn open_explorer(&mut self) {
        let open = self.open_states();
        let mut e = explorer::Explorer::new(self.cwd(), open);
        e.set_folds(self.list_prefs.1.clone());
        self.explorer = Some(e);
    }

    /// Reopen a stored conversation: switch to its tab if it is open, else use this tab when it is
    /// still blank, else a new one.
    fn open_session(&mut self, id: String, title: String, cwd: Option<String>) {
        if let Some(i) = self.sessions.iter().position(|s| s.session_id.as_deref() == Some(id.as_str())) {
            self.goto(i);
            return;
        }
        self.open_in_tab(id, title, cwd);
    }

    /// → in a list: the conversation takes the tab in front (its backend stops, as with `/clear`). One that
    /// is already open in another tab is switched to; a tab mid-turn is not thrown away.
    fn open_here(&mut self, id: String, title: String, cwd: Option<String>) {
        if self.sessions.iter().any(|s| s.session_id.as_deref() == Some(id.as_str())) {
            return self.open_session(id, title, cwd);
        }
        let s = &self.sessions[self.active];
        if s.busy || !s.pending.is_empty() {
            return self.open_session(id, title, cwd); // a working tab is not thrown away: new tab instead
        }
        self.s().clear();
        self.open_in_tab(id, title, cwd);
    }

    /// Resume `id` in this tab if it is blank, else in a new one, working in `cwd`.
    fn open_in_tab(&mut self, id: String, title: String, cwd: Option<String>) {
        let blank = {
            let s = &self.sessions[self.active];
            s.items.is_empty() && s.session_id.is_none() && s.backend.is_none()
        };
        if !blank {
            self.new_tab();
        }
        if !title.is_empty() {
            self.s().title = title.chars().take(18).collect();
        }
        // A conversation from another project starts with that project's profile, else the default one
        // (a tab you already set up by hand is left as it is).
        if !self.profile_forced && let Some(dir) = cwd.as_deref() {
            let own = project_profile(dir);
            if own.is_some() || !blank {
                let p = own.unwrap_or_else(|| self.default_profile.clone());
                self.s().profile = p;
            }
        }
        self.s().cwd = cwd;
        self.s().resume(id);
        self.files_at = None;
    }

    fn handle_key(&mut self, k: KeyEvent) {
        self.handle_key_inner(k);
        self.sync_list_prefs();
    }

    /// Whichever list the key touched, make the other agree and save: folds and grouping outlive both.
    fn sync_list_prefs(&mut self) {
        let from_ui = self.explorer.as_ref().or(self.dock.as_ref()).map(|e| e.folds().to_vec());
        let prefs = (explorer::group(), from_ui.unwrap_or_else(|| self.list_prefs.1.clone()));
        if prefs == self.list_prefs {
            return;
        }
        self.list_prefs = prefs;
        for e in self.dock.iter_mut().chain(self.explorer.iter_mut()) {
            e.set_folds(self.list_prefs.1.clone());
        }
        if !cfg!(test) && let Some(p) = &self.config_path {
            let _ = config::set_str(p, "session_group", &self.list_prefs.0.to_string());
            let _ = config::set_value(p, "session_folds", serde_json::json!(self.list_prefs.1));
        }
    }

    fn handle_key_inner(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if let Some(ex) = self.explorer.as_mut() {
            match ex.key(k) {
                explorer::Outcome::Stay => {}
                explorer::Outcome::Close => {
                    self.explorer = None;
                    if self.dock_mode {
                        self.show_dock(true); // Esc from the full screen steps back to the docked list
                    }
                }
                other => {
                    let close = !matches!(other, explorer::Outcome::Refused(_));
                    if close {
                        self.explorer = None;
                    }
                    self.explorer_outcome(other);
                }
            }
            return;
        }
        if self.color_picker.is_some() {
            return self.color_key(k);
        }
        if self.model_picker.is_some() {
            return self.picker_key(k);
        }
        if self.rewind.is_some() {
            return self.rewind_key(k);
        }
        if self.dock_focus && self.dock.is_some() && self.settings.is_none() && self.tabcfg.is_none() && self.instr.is_none() && self.perms.is_none() && self.opt.is_none() && !self.usage_open && self.ctx_ui.is_none() && self.lanes.is_none() && self.queue_ui.is_none() && self.sched_ui.is_none() && self.search.is_none() && self.sessions[self.active].pending.is_empty() {
            use KeyCode::*;
            if k.code == Char('t') && ctrl {
                self.new_tab();
                (self.dock_focus, self.dock_peek) = (false, false); // typing now starts the new conversation
                return;
            }
            if matches!(k.code, Up | Down | PageUp | PageDown | Home | End | Enter | Right | Esc | Left | Tab) && !ctrl && !alt {
                return self.dock_key(k);
            }
            (self.dock_focus, self.dock_peek) = (false, false); // any other key is for the input box
        }
        if self.settings.is_some() {
            return self.settings_key(k);
        }
        if self.tabcfg.is_some() {
            return self.tabcfg_key(k);
        }
        if self.instr.is_some() {
            return self.instr_key(k);
        }
        if self.perms.is_some() {
            return self.perms_key(k);
        }
        if self.opt.is_some() {
            return self.opt_key(k);
        }
        if let Some(mut ui) = self.ctx_ui.take() {
            let ctx = self.sessions[self.active].usage.ctx.clone();
            match ui.key(k, ctx.as_ref().map_or(0, context::count)) {
                context::Out::Keep => self.ctx_ui = Some(ui),
                context::Out::Close => {}
                context::Out::Refresh => {
                    self.ctx_ui = Some(ui);
                    self.ask_usage();
                }
                context::Out::Act => match ctx.and_then(|c| context::action(&c, ui.sel)) {
                    Some((cmd, _)) => {
                        self.input = cmd.into();
                        self.cursor = self.input.chars().count();
                        self.submit();
                    }
                    None => self.ctx_ui = Some(ui),
                },
            }
            return;
        }
        if let Some(mut ui) = self.lanes.take() {
            let all = lanes::collect(&self.sessions[self.active].items, &self.sessions[self.active].shells, self.cwd());
            let keep = ui.key(k, all.len());
            if let Some(id) = ui.stop.take().and_then(|i| all.get(i)?.stop.clone()) {
                self.s().control("stop_task", json!({"task_id": id}));
                self.flash("stopping the command");
            }
            if keep {
                self.lanes = Some(ui);
            }
            return;
        }
        if self.sched_ui.is_some() {
            return self.schedule_key(k);
        }
        if self.queue_ui.is_some() {
            return self.queue_key(k);
        }
        if self.search.is_some() {
            return self.search_key(k);
        }
        if self.usage_open {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => self.usage_open = false,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.usage_open = false,
                KeyCode::Char('r') => self.open_usage(),
                _ => {}
            }
            return;
        }
        let rows = self.completions();
        if !rows.is_empty() {
            let sel = self.sel.min(rows.len() - 1);
            match k.code {
                KeyCode::Tab => return self.accept_completion(),
                KeyCode::Up | KeyCode::Down => return self.sel = panel::cycle(sel, rows.len(), k.code == KeyCode::Up),
                // Enter completes a partial name; once the name is complete it runs the command.
                KeyCode::Enter
                    if !alt
                        && !k.modifiers.contains(KeyModifiers::SHIFT)
                        && (rows[sel].prefix == '@' || self.input != format!("/{}", rows[sel].name)) =>
                {
                    return self.accept_completion();
                }
                KeyCode::Esc => return self.comp_off = Some(self.input.clone()),
                _ => {}
            }
        }
        if !matches!(k.code, KeyCode::Char('c')) || !ctrl {
            self.quit_armed = None;
        }
        if k.code != KeyCode::Esc {
            self.esc_armed = None;
        }

        // A permission or question panel owns the keyboard, except keys the app keeps (ctrl/alt chords for
        // tabs, interrupt..., and page keys that scroll the transcript behind it).
        let scrolling = matches!(k.code, KeyCode::PageUp | KeyCode::PageDown);
        if !ctrl && !alt && !scrolling && !self.sessions[self.active].pending.is_empty() {
            return self.prompt_key(k);
        }

        if self.sched_at.is_some() && k.code == KeyCode::Esc {
            self.sched_at = None;
            return self.flash("schedule cancelled");
        }
        // The spinner says "esc to interrupt": with nothing typed there is no insert mode to leave, so Esc does that.
        let interrupt_now = k.code == KeyCode::Esc && self.s().busy && self.input.is_empty();
        if self.vim.on && !interrupt_now && self.vim_key(k) {
            return;
        }
        match k.code {
            KeyCode::Char('f') if ctrl => self.open_search(),
            KeyCode::Char('g') if ctrl => self.edit_request = true,
            KeyCode::Char('q') if alt => self.queue_ui = Some(Default::default()),
            KeyCode::Char('c') if ctrl => {
                if self.s().busy {
                    self.s().interrupt();
                } else if !self.input.is_empty() {
                    self.set_input(String::new());
                } else if self.quit_armed.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    self.quit = true;
                } else {
                    self.quit_armed = Some(Instant::now());
                    self.flash("press ctrl+c again to quit");
                }
            }
            KeyCode::Esc if self.s().busy => self.s().interrupt(),
            // Esc Esc on an empty box goes back to an earlier message, as in Claude Code.
            KeyCode::Esc if self.input.is_empty() => {
                if self.esc_armed.take().is_some_and(|t| t.elapsed() < Duration::from_millis(800)) {
                    self.open_rewind();
                } else if !self.s().rewind_points().is_empty() {
                    self.esc_armed = Some(Instant::now());
                    self.flash("esc again to rewind");
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.close_tab_asking(),
            KeyCode::Char('t') if ctrl => self.new_tab(),
            // As in Claude Code: search what you sent before. The session list is ← (or /sessions).
            KeyCode::Char('r') if ctrl => {
                self.open_search();
                if let Some(ui) = self.search.as_mut() {
                    ui.history = true;
                    ui.refresh(&self.sessions[self.active].items, &self.history);
                }
            }
            KeyCode::Char('v') if ctrl => {
                self.paste_image("no image on the clipboard (ctrl+v pastes images; paste text with your terminal's paste)");
            }
            KeyCode::Char('n') if ctrl => self.switch(1),
            KeyCode::Char('p') if ctrl => self.switch(-1),
            KeyCode::Char(c @ '1'..='9') if alt => {
                let i = c as usize - '1' as usize;
                if i < self.sessions.len() {
                    self.goto(i);
                }
            }
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                for s in &mut self.sessions {
                    s.items.iter_mut().for_each(|i| (i.cache, i.flipped) = (None, false));
                }
            }
            KeyCode::Char('o') if alt => {
                if !self.s().flip_call(None) {
                    self.flash("no tool call to fold");
                }
            }
            KeyCode::Char('y') if ctrl => {
                if self.copy_selection() {
                    self.drag = None;
                } else {
                    self.copy_last("")
                }
            }
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => self.insert("\n"),
            KeyCode::Enter => self.submit(),
            KeyCode::BackTab => {
                let modes = tabcfg::modes();
                let next = modes[(modes.iter().position(|m| *m == self.s().mode).map_or(0, |i| i + 1)) % modes.len()];
                self.set_mode(next);
            }
            KeyCode::Char('a') if ctrl => self.cursor = self.line_bounds().0,
            KeyCode::Char('e') if ctrl => self.cursor = self.line_bounds().1,
            // ctrl+u / ctrl+k cut to the start / end of the line; at its edge they take the line break with them.
            KeyCode::Char('u') if ctrl => {
                let a = self.line_bounds().0;
                self.delete_range(a.saturating_sub((a == self.cursor) as usize), self.cursor)
            }
            KeyCode::Char('w') if ctrl => self.delete_range(self.word_left(), self.cursor),
            KeyCode::Backspace if alt => self.delete_range(self.word_left(), self.cursor),
            KeyCode::Char('d') if alt => self.delete_range(self.cursor, self.word_right()),
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
            KeyCode::Char('k') if ctrl => {
                let b = self.line_bounds().1;
                self.delete_range(self.cursor, if b == self.cursor { (b + 1).min(self.input.chars().count()) } else { b })
            }
            KeyCode::Char('p') if alt => self.open_model_picker(), // as in Claude Code
            KeyCode::Char('b') if ctrl => {
                let said = self.s().background_shell();
                self.flash(said);
            }
            KeyCode::Char('l') if ctrl => self.clear_request = true,
            // An unbound alt+letter is a shortcut that does not exist here, not text to type.
            KeyCode::Char(c) if !ctrl && !(alt && c.is_ascii_alphabetic()) => {
                self.insert(c.encode_utf8(&mut [0; 4]));
                self.warm();
            }
            KeyCode::Backspace if self.cursor > 0 && self.token_len(true).is_some() => {
                let len = self.token_len(true).unwrap();
                let (a, b) = (self.byte_at(self.cursor - len), self.byte_at(self.cursor));
                self.input.replace_range(a..b, "");
                self.cursor -= len;
            }
            KeyCode::Delete if self.token_len(false).is_some() => {
                let len = self.token_len(false).unwrap();
                let (a, b) = (self.byte_at(self.cursor), self.byte_at(self.cursor + len));
                self.input.replace_range(a..b, "");
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_at(self.cursor);
                self.input.remove(at);
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let at = self.byte_at(self.cursor);
                self.input.remove(at);
            }
            KeyCode::Left if alt || ctrl => self.cursor = self.word_left(),
            KeyCode::Right if alt || ctrl => self.cursor = self.word_right(),
            // As in Claude Code: ← with nothing to the left of the cursor goes to the session list.
            KeyCode::Left if self.cursor == 0 && !ctrl && !alt => {
                if self.dock_mode { self.show_dock(true) } else { self.open_explorer() }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home if ctrl => self.s().scroll = usize::MAX / 2,
            KeyCode::End if ctrl => self.s().scroll = 0,
            KeyCode::Home => self.cursor = self.line_bounds().0,
            KeyCode::End => self.cursor = self.line_bounds().1,
            KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => self.s().scroll += 1,
            KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => {
                self.s().scroll = self.s().scroll.saturating_sub(1)
            }
            KeyCode::PageUp => self.s().scroll += self.s().view_h.saturating_sub(2).max(1),
            KeyCode::PageDown => self.s().scroll = self.s().scroll.saturating_sub(self.s().view_h.saturating_sub(2).max(1)),
            // ↑/↓ move through a message that is more than one row; from its first / last row they walk history.
            KeyCode::Up if !self.move_row(-1) => self.history_step(-1),
            KeyCode::Down if !self.move_row(1) => self.history_step(1),
            _ => {}
        }
    }

    /// A key for vim's modes. True when it was used up; false hands it on to the ordinary keys.
    fn vim_key(&mut self, k: KeyEvent) -> bool {
        let mut t: Vec<char> = self.input.chars().collect();
        let mut cur = self.cursor.min(t.len());
        let out = self.vim.key(k, &mut t, &mut cur);
        let text: String = t.iter().collect();
        if text != self.input {
            self.input = text;
        }
        self.cursor = cur;
        match out {
            vim::Out::Pass => return false,
            vim::Out::Row(d) => {
                (0..d.unsigned_abs()).for_each(|_| _ = self.move_row(d.signum()));
                self.vim_settle();
            }
            vim::Out::Search => self.open_search(),
            vim::Out::Done | vim::Out::Undo | vim::Out::Redo => {}
        }
        true
    }

    /// In vim's normal and visual modes the cursor sits on a character, not after the last one.
    fn vim_settle(&mut self) {
        if self.vim.on {
            let t: Vec<char> = self.input.chars().collect();
            self.vim.settle(&t, &mut self.cursor);
        }
    }

    /// The editor to run: `editor` in config.json, else `$VISUAL`, `$EDITOR`, `vi`.
    fn editor(&self) -> String {
        self.editor_cmd.clone().unwrap_or_else(editor_command)
    }

    /// The editor owns the keyboard unless a prompt or panel is asking you something (full screen: always).
    fn pane_has_keyboard(&self) -> bool {
        self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Edit || (self.sessions[self.active].pending.is_empty() && !self.overlay_open()))
    }

    /// Start the editor on `text`. Err when it cannot (no pty): the caller falls back to the box.
    fn open_pane(&mut self, kind: pane::Use, text: &str) -> io::Result<()> {
        self.pane_seq += 1;
        let (w, h) = terminal::size().unwrap_or((80, 24));
        let rows = if kind == pane::Use::Edit { h.saturating_sub(1) } else { ui::pane_rows(h) };
        let pane = pane::Pane::spawn(self.pane_seq, kind, &self.editor(), text, (rows, w), self.tx.clone(), AppEvent::Pane)?;
        self.pane = Some(pane);
        self.pane_shape = u8::MAX;
        self.pane_typed = false;
        Ok(())
    }

    /// Input mode editor: the message area IS the editor. It is open whenever the box is free, and a fresh one
    /// replaces it after each send. (Claude itself still starts only when you type.)
    fn keep_editor_open(&mut self) {
        if !self.editor_mode {
            // Turned off: close an idle send editor (anything typed in it is lost, as with ctrl+\).
            if self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send) {
                self.pane = None;
                self.pane_closed();
            }
            self.pane_failed = false;
            return;
        }
        if self.pane.is_some() || self.pane_failed || self.overlay_open() || !self.sessions[self.active].pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.input);
        if let Err(e) = self.open_pane(pane::Use::Send, &text) {
            self.input = text;
            self.pane_failed = true;
            self.flash(format!("could not start the editor ({e}): using the plain box"));
        } else {
            self.cursor = 0;
        }
    }

    /// The editor is for typing, which is the moment to start Claude.
    fn warm_active(&mut self) {
        let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
        self.s().warm(&launch, &tx);
    }

    fn pane_key(&mut self, k: KeyEvent) {
        // The one key the editor never sees: a way out if it hangs or you change your mind. (A terminal that
        // cannot tell ctrl+\ apart sends ctrl+4.)
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('\\' | '4')) {
            self.pane = None;
            self.pane_closed();
            let old = self.pane_old.take();
            if let Some((text, cursor)) = old {
                self.set_input(text);
                self.cursor = cursor;
            }
            return self.flash("editor closed: nothing sent");
        }
        // ctrl+z would suspend the editor, and nothing here could wake it.
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('z') {
            return self.flash("ctrl+z is not passed to the editor (it would suspend it)");
        }
        self.pane_typed = true;
        if self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send) {
            self.warm_active();
        }
        if let Some(p) = self.pane.as_mut() {
            p.key(k);
        }
    }

    fn on_pane(&mut self, id: u64, msg: pane::Msg) {
        let Some(p) = self.pane.as_mut().filter(|p| p.id == id) else { return };
        match msg {
            pane::Msg::Bytes(b) => {
                p.feed(&b);
                self.sync_pane_cursor();
            }
            pane::Msg::Ended => self.finish_pane(),
        }
    }

    /// Ask the terminal for the cursor shape the editor wants (vim: block in normal mode, bar in insert).
    fn sync_pane_cursor(&mut self) {
        let Some(shape) = self.pane.as_ref().map(|p| p.cursor_shape()) else { return };
        if shape != self.pane_shape {
            self.pane_shape = shape;
            if self.tty {
                let _ = write!(io::stdout(), "\x1b[{shape} q");
                let _ = io::stdout().flush();
            }
        }
    }

    /// The editor is gone: give the cursor back to the box.
    fn pane_closed(&mut self) {
        if self.tty {
            let _ = write!(io::stdout(), "\x1b[0 q");
            let _ = io::stdout().flush();
        }
        self.cursor_mode = None;
        self.vim_shape();
    }

    /// The editor exited: send (input mode editor) or return (ctrl+g) what it left in the file.
    fn finish_pane(&mut self) {
        let Some(p) = self.pane.take() else { return };
        let kind = p.kind;
        let out = p.finish();
        self.pane_closed();
        let old = self.pane_old.take();
        let text = match out.text {
            Ok(t) => t,
            Err(e) => return self.flash(format!("editor: could not read the message back: {e}")),
        };
        match kind {
            pane::Use::Send if !out.ok => {
                // :cq, a crash, a kill: not a send, but what was written is not thrown away.
                self.set_input(text);
                self.flash(format!("editor exited with {}: nothing sent; your text is in the box", out.status));
            }
            pane::Use::Send if text.trim().is_empty() => self.flash("empty message: nothing sent"),
            pane::Use::Send => {
                self.set_input(text);
                self.submit();
            }
            pane::Use::Edit if !out.ok => self.flash(format!("editor exited with {}; your message is as it was", out.status)),
            pane::Use::Edit => {
                let old = old.unwrap_or_default();
                if text != old.0 {
                    self.undo.push(old);
                    self.redo.clear();
                    self.set_input(text);
                    self.vim.mode = vim::Mode::Insert;
                    self.flash("message edited");
                } else {
                    self.flash("message unchanged");
                }
            }
        }
        self.vim_shape();
    }

    /// Ask the terminal for a block cursor in normal mode and a bar in insert mode.
    fn vim_shape(&mut self) {
        let want = self.vim.on.then_some(self.vim.mode);
        if want == self.cursor_mode {
            return;
        }
        self.cursor_mode = want;
        if self.tty {
            let shape = match want {
                None => 0,
                Some(vim::Mode::Insert) => 6,
                Some(_) => 2,
            };
            let _ = write!(io::stdout(), "\x1b[{shape} q");
            let _ = io::stdout().flush();
        }
    }

    fn open_search(&mut self) {
        let scroll = self.s().scroll;
        let mut ui = search::Ui::new(scroll);
        ui.refresh(&self.sessions[self.active].items, &self.history);
        self.search = Some(ui);
    }

    fn search_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.search.take() else { return };
        let out = ui.key(k, &self.sessions[self.active].items, &self.history);
        let at = ui.reveal();
        match out {
            search::Outcome::Stay => {
                self.s().reveal = at.or(self.s().reveal);
                self.search = Some(ui);
            }
            search::Outcome::Close => self.s().scroll = ui.scroll,
            search::Outcome::Go => {
                let s = self.s();
                s.reveal = at;
                (s.find_query, s.found) = (ui.query.to_lowercase(), at);
            }
            search::Outcome::Recall(i) => {
                let text = self.history[i].clone();
                self.set_input(text);
            }
        }
    }

    /// Put a message in this tab's queue, with the pictures it mentions.
    fn queue_message(&mut self, text: String, due: Option<SystemTime>) {
        let used = image::tokens_in(&text);
        let (keep, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.s().attachments).into_iter().partition(|a| used.contains(&a.n));
        gone.iter().for_each(|a| a.preview.iter().for_each(|p| image::delete(p.id)));
        let keep = keep
            .into_iter()
            .map(|mut a| {
                a.preview.take().iter().for_each(|p| image::delete(p.id)); // the box no longer shows it
                a
            })
            .collect();
        self.undo.clear();
        self.redo.clear();
        let s = self.s();
        if due.is_none() {
            s.queue_hold = false; // sending something new means carrying on
        }
        s.queue.push(session::Queued { text, atts: keep, due });
        let n = s.queue.len();
        self.flash(match due {
            Some(d) => format!("scheduled for {} · alt+q to manage", queue::clock(d)),
            None => format!("queued ({n}) · alt+q to manage"),
        });
    }

    /// Send the next waiting message of every tab that is free to take one.
    fn drain_queues(&mut self) {
        for i in 0..self.sessions.len() {
            let s = &mut self.sessions[i];
            if s.busy || s.queue_hold || s.rewinding || !s.pending.is_empty() {
                continue;
            }
            // The first message that is not waiting for a time of its own.
            let now = SystemTime::now();
            let Some(at) = s.queue.iter().position(|q| q.due.is_none_or(|d| d <= now)) else { continue };
            let q = s.queue.remove(at);
            self.send_queued(i, q);
        }
    }

    fn send_queued(&mut self, i: usize, q: session::Queued) {
        let (launch, tx) = (self.launch_for(i), self.tx.clone());
        let mentioned = self.mentioned_images(&q.text);
        let mine = std::mem::replace(&mut self.sessions[i].attachments, q.atts);
        self.sessions[i].submit(q.text, &mentioned, &launch, &tx);
        self.sessions[i].attachments = mine;
    }

    fn queue_key(&mut self, k: KeyEvent) {
        use queue::Act;
        let Some(mut ui) = self.queue_ui.take() else { return };
        let i = self.active;
        let act = ui.key(k, &mut self.sessions[i].queue);
        let keep = match act {
            Act::Close => false,
            Act::None => true,
            Act::Send(n) => {
                // It goes first; if Claude is mid-turn that turn is stopped, and the message follows its end.
                let s = &mut self.sessions[i];
                let mut q = s.queue.remove(n);
                q.due = None;
                s.queue.insert(0, q);
                s.interrupt();
                s.queue_hold = false;
                self.drain_queues();
                true
            }
            Act::Steer(n) => {
                let mut q = self.sessions[i].queue.remove(n);
                q.due = None;
                self.send_queued(i, q);
                true
            }
            Act::Edit(_) if !self.input.is_empty() => {
                self.flash("send or clear the message box first: editing a queued message needs it");
                true
            }
            Act::Edit(n) => {
                let q = self.sessions[i].queue.remove(n);
                self.set_input(q.text);
                self.sessions[i].attachments.extend(q.atts);
                false
            }
            Act::Remove(n) => {
                self.sessions[i].queue.remove(n);
                true
            }
            Act::Clear => {
                self.sessions[i].queue.clear();
                true
            }
            Act::Schedule(n) => {
                let mut s = schedule::Ui::new("");
                (s.target, s.from_queue) = (Some(n), true);
                self.sched_ui = Some(s);
                false
            }
            Act::Pause => {
                let s = &mut self.sessions[i];
                s.queue_hold = !s.queue_hold;
                self.drain_queues();
                true
            }
        };
        if keep {
            self.queue_ui = Some(ui);
        }
    }

    fn schedule_key(&mut self, k: KeyEvent) {
        use schedule::Act;
        let Some(mut ui) = self.sched_ui.take() else { return };
        let i = self.active;
        let unset = ui.target.and_then(|n| self.sessions[i].queue.get(n)).is_some_and(|q| q.due.is_some());
        let done = match ui.key(k, SystemTime::now(), unset) {
            Act::None => false,
            Act::Close => true,
            Act::Bad => {
                self.flash("not a time: try 45m, 2h30m or 17:30");
                false
            }
            Act::Pick(due) => {
                if let Some(n) = ui.target {
                    self.sessions[i].queue.get_mut(n).iter_mut().for_each(|q| q.due = due);
                    self.flash(due.map_or("no longer scheduled: it goes when the turn ends".into(), |d| format!("scheduled for {}", queue::clock(d))));
                } else if let (Some(msg), Some(due)) = (ui.msg.take(), due) {
                    self.queue_message(msg, Some(due));
                } else {
                    self.sched_at = due;
                    self.flash("now type the message: enter schedules it, esc cancels");
                }
                true
            }
        };
        if !done {
            self.sched_ui = Some(ui);
        } else if ui.from_queue {
            self.queue_ui = Some(Default::default());
        }
    }

    /// The message box's current line (between line breaks) as (start, end) char positions.
    fn line_bounds(&self) -> (usize, usize) {
        let chars: Vec<char> = self.input.chars().collect();
        let a = chars[..self.cursor.min(chars.len())].iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
        (a, chars[a..].iter().position(|c| *c == '\n').map_or(chars.len(), |i| a + i))
    }

    /// Move the cursor one row up or down in the box, keeping its column. False when there is no such row
    /// and the key is history's (↓ with no history shown just goes to the end).
    fn move_row(&mut self, d: isize) -> bool {
        let (rows, (col, row)) = ui::input_layout(&self.input, self.cursor, self.input_w.max(1));
        let to = row as isize + d;
        if to < 0 {
            return false;
        }
        let to = to as usize;
        if to >= rows.len() {
            if self.hist_pos.is_some() {
                return false;
            }
            self.cursor = self.input.chars().count();
            return true;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = 0;
        for r in &rows[..to] {
            start += r.chars().count();
            start += (chars.get(start) == Some(&'\n')) as usize;
        }
        let (mut w, mut n) = (0, 0);
        for ch in rows[to].chars() {
            w += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w > col {
                break;
            }
            n += 1;
        }
        self.cursor = start + n;
        true
    }

    fn history_step(&mut self, d: isize) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        let pos = match (self.hist_pos, d) {
            (None, -1) => {
                self.hist_draft = self.input.clone();
                Some(n - 1)
            }
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(p), -1) => Some(p - 1),
            (Some(p), _) if p + 1 < n => Some(p + 1),
            (Some(_), _) => None,
        };
        self.hist_pos = pos;
        let text = match pos {
            Some(p) => self.history[p].clone(),
            None => std::mem::take(&mut self.hist_draft),
        };
        self.set_input(text);
    }

    /// Route a key to the panel at the front of this tab's prompt queue.
    fn prompt_key(&mut self, k: KeyEvent) {
        enum Act {
            Nothing,
            Choose(Choice),
            Answers(Vec<(String, String)>),
        }
        let act = {
            let Some(p) = self.sessions[self.active].pending.front_mut() else { return };
            if p.is_question() {
                let qs = p.questions().to_vec();
                match p.ui.question_key(&qs, k) {
                    prompt::Outcome::Stay => Act::Nothing,
                    prompt::Outcome::Decline => Act::Choose(Choice::Deny),
                    prompt::Outcome::Submit(a) => Act::Answers(a),
                }
            } else {
                let has_always = p.suggestions.as_array().is_some_and(|a| !a.is_empty());
                p.ui.permission_key(has_always, k).map_or(Act::Nothing, Act::Choose)
            }
        };
        match act {
            Act::Nothing => {}
            Act::Choose(c) => self.s().answer(c),
            Act::Answers(a) => self.s().submit_answers(a),
        }
    }

    /// Image files named as `@path` in a message: Claude Code reads them, and we show them inline.
    fn mentioned_images(&self, text: &str) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = Vec::new();
        for word in text.split_whitespace().filter_map(|w| w.strip_prefix('@')) {
            let word = word.trim_end_matches([',', '.', ';', ':', ')', '!', '?']);
            if image::media_type(word).is_none() {
                continue;
            }
            let p = PathBuf::from(self.cwd()).join(word);
            if p.is_file() && !found.contains(&p) {
                found.push(p);
            }
        }
        found
    }

    /// Models for the picker: what this tab's backend reported, else what we remembered.
    pub fn model_list(&self) -> &[ModelInfo] {
        let own = &self.sessions[self.active].models;
        if own.is_empty() { &self.known_models } else { own }
    }

    /// Index in the model list of the model this tab is on: the one the user picked, else the first
    /// whose id matches (several aliases can resolve to one id).
    pub fn current_model(&self) -> Option<usize> {
        let s = &self.sessions[self.active];
        let list = self.model_list();
        s.model_pref
            .as_ref()
            .and_then(|p| list.iter().position(|m| &m.value == p))
            .or_else(|| list.iter().position(|m| !s.model.is_empty() && m.resolved == s.model))
    }

    /// Ask Claude Code for the context breakdown and plan limits (free: no model call) and show the panel,
    /// which fills in when the replies arrive.
    fn open_usage(&mut self) {
        self.usage_open = true;
        self.ask_usage();
    }

    fn ask_usage(&mut self) {
        let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
        let s = self.s();
        s.usage = Default::default();
        s.warm(&launch, &tx);
        s.control("get_context_usage", json!({}));
        s.control("get_usage", json!({}));
    }

    /// `/optimize [level|off|now]`.
    fn optimize_cmd(&mut self, arg: &str) {
        let cwd = self.cwd().to_string();
        match arg {
            "off" => {
                let had = optimize::clear(&cwd);
                self.flash(if had { "optimize removed for this project: new tabs load everything again" } else { "nothing to remove: this project has no saved optimize settings" });
            }
            "now" => {
                let k = self.s().ctx_tokens / 1000;
                match self.s().restart_later() {
                    Ok(()) => {
                        self.s().unpinned = true;
                        self.s().note(format!("re-rendering this conversation with the project's optimize settings; the next message re-caches up to ~{k}k tokens"));
                    }
                    Err(e) => self.flash(e),
                }
            }
            _ => {
                let level = optimize::Level::parse(arg).or_else(|| optimize::load(&cwd).and_then(|o| o.level)).unwrap_or(optimize::Level::Balanced);
                if !arg.is_empty() && optimize::Level::parse(arg).is_none() {
                    return self.flash("usage: /optimize [light|balanced|aggressive|off|now]");
                }
                self.opt = Some(optimize::Ui::open(&cwd, level));
                self.ask_usage();
            }
        }
    }

    fn opt_poll(&mut self) {
        let Some(ui) = self.opt.as_mut() else { return };
        let u = &self.sessions[self.active].usage;
        let (skills, total) = u.ctx.as_ref().map_or((None, 0), |c| (Some(c.skill_list.as_slice()), c.total));
        ui.poll(skills, total);
        ui.poll_review();
    }

    fn opt_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.opt.take() else { return };
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) || ctrl_c {
            return;
        }
        if ui.loading {
            self.opt = Some(ui);
            return;
        }
        let n = ui.items.len();
        match k.code {
            KeyCode::Up | KeyCode::Down if n > 0 => ui.sel = panel::cycle(ui.sel, n, k.code == KeyCode::Up),
            KeyCode::Left | KeyCode::BackTab => ui.step_level(-1),
            KeyCode::Right | KeyCode::Tab => ui.step_level(1),
            KeyCode::Char('c') if !ctrl_c => {
                let cwd = self.cwd().to_string();
                ui.ask_claude(&self.launch.bin, &self.launch.env, &cwd);
            }
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                if let Some(it) = ui.items.get_mut(ui.sel) {
                    it.on = !it.on;
                }
            }
            KeyCode::Enter => {
                let cwd = self.cwd().to_string();
                match optimize::save(&cwd, &ui.saved()) {
                    Err(e) => self.flash(format!("could not save: {e}")),
                    Ok(()) => {
                        let started = self.sessions[self.active].ctx_tokens > 0;
                        let cold = self.sessions[self.active].last_req_at.is_none_or(|t| t.elapsed() > self.sessions[self.active].cache_ttl);
                        let msg = format!("optimize {} saved (≈ −{} at start). ", ui.level.name(), crate::usage::tokens(ui.saves()));
                        if !started || cold {
                            if self.s().restart_later().is_ok() {
                                self.s().unpinned = true;
                            }
                            self.flash(msg + "applied to this tab");
                        } else {
                            let k = self.s().ctx_tokens / 1000;
                            self.flash(msg + &format!("New tabs use it; this conversation keeps its prompt. `/optimize now` re-renders it (re-caches ~{k}k)"));
                        }
                        return;
                    }
                }
            }
            _ => {}
        }
        self.opt = Some(ui);
    }

    fn perms_key(&mut self, k: KeyEvent) {
        use permissions::Stage;
        let Some(mut ui) = self.perms.take() else { return };
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        ui.note = None;
        match ui.stage.clone() {
            Stage::Typing(mut text) => {
                match k.code {
                    KeyCode::Esc => ui.stage = Stage::List,
                    KeyCode::Enter if !text.trim().is_empty() => ui.stage = Stage::Where(text.trim().to_string(), 0),
                    KeyCode::Backspace => {
                        text.pop();
                        ui.stage = Stage::Typing(text);
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        text.push(c);
                        ui.stage = Stage::Typing(text);
                    }
                    _ => {}
                }
                if ctrl_c {
                    ui.stage = Stage::List;
                }
            }
            Stage::Where(rule, w) => match k.code {
                KeyCode::Esc => ui.stage = Stage::Typing(rule),
                KeyCode::Up => ui.stage = Stage::Where(rule, (w + 2) % 3),
                KeyCode::Down | KeyCode::Tab => ui.stage = Stage::Where(rule, (w + 1) % 3),
                KeyCode::Enter => {
                    let scope = permissions::Scope::ALL[w];
                    let res = permissions::Scope::file(scope, self.cwd()).ok_or("no settings location".to_string()).and_then(|f| permissions::add(&f, ui.tab, &rule));
                    match res {
                        Ok(true) => ui.stage = Stage::List,
                        Ok(false) => {
                            ui.note = Some(format!("{rule} is already in {} settings", scope.name()));
                            ui.stage = Stage::List;
                        }
                        Err(e) => ui.note = Some(e),
                    }
                    ui.reload(&self.sessions[self.active]);
                    ui.sel = ui.shown().iter().position(|r| r.text == rule).map_or(0, |i| i + 1);
                }
                _ => {}
            },
            stage => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => return,
                _ if ctrl_c => return,
                KeyCode::Left | KeyCode::BackTab => ui.switch(-1),
                KeyCode::Right | KeyCode::Tab => ui.switch(1),
                KeyCode::Up => {
                    ui.sel = (ui.sel + ui.shown().len()) % (ui.shown().len() + 1);
                    ui.stage = Stage::List;
                }
                KeyCode::Down => {
                    ui.sel = (ui.sel + 1) % (ui.shown().len() + 1);
                    ui.stage = Stage::List;
                }
                KeyCode::Enter if ui.sel == 0 => ui.stage = Stage::Typing(String::new()),
                KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace if ui.sel > 0 => {
                    if let Some(r) = ui.at().cloned() {
                        match r.source {
                            permissions::Source::Tab => ui.note = Some("this tab's own rule: change it in /session".into()),
                            permissions::Source::File(scope) if matches!(stage, Stage::Confirm(n) if n + 1 == ui.sel) => {
                                let res = scope.file(self.cwd()).ok_or("no settings location".to_string()).and_then(|f| permissions::remove(&f, r.behavior, &r.text));
                                if let Err(e) = res {
                                    ui.note = Some(e);
                                }
                                ui.stage = Stage::List;
                                ui.reload(&self.sessions[self.active]);
                            }
                            permissions::Source::File(_) => ui.stage = Stage::Confirm(ui.sel - 1),
                        }
                    }
                }
                _ => {}
            },
        }
        self.perms = Some(ui);
    }

    fn instr_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.instr.take() else { return };
        let rows = ui.rows();
        let sel = ui.sel.min(rows.len().saturating_sub(1));
        let close = matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL));
        if close {
            if ui.dirty {
                if let Some(f) = instructions::rules_path() {
                    instructions::save_rules(&f, &ui.rules);
                }
                // A tab that has not started yet picks the change up at once; a started one keeps the
                // prompt it began with (Claude Code pins it), so say so.
                if self.sessions[self.active].ctx_tokens == 0 && self.s().restart_later().is_ok() {
                    self.flash("instructions updated for this tab");
                } else {
                    self.flash("instructions updated: new tabs use them (this conversation keeps its start-up prompt)");
                }
            }
            return;
        }
        if let Some(&row) = rows.get(sel) {
            match k.code {
                KeyCode::Up | KeyCode::Down => ui.sel = panel::cycle(sel, rows.len(), k.code == KeyCode::Up),
                KeyCode::Right | KeyCode::Char('l') => {
                    if let instructions::Row::File(f) = row {
                        ui.open.insert(f);
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    let f = match row { instructions::Row::File(f) | instructions::Row::Sec(f, _) => f };
                    ui.open.remove(&f);
                    ui.sel = ui.rows().iter().position(|r| *r == instructions::Row::File(f)).unwrap_or(0);
                }
                KeyCode::Char('b') => ui.toggle_brief(row),
                KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Char('x') => {
                    if let Err(e) = ui.toggle(row) {
                        self.flash(e);
                    }
                }
                _ => {}
            }
        }
        self.instr = Some(ui);
    }

    fn tabcfg_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.tabcfg.take() else { return };
        let rows = tabcfg::rows(&self.sessions[self.active]);
        let sel = ui.sel.min(rows.len() - 1);
        if let Some(text) = ui.adding.as_mut() {
            match k.code {
                KeyCode::Esc => ui.adding = None,
                KeyCode::Enter => {
                    let rule = text.trim().to_string();
                    ui.adding = None;
                    if !rule.is_empty() {
                        self.s().set_rule(&rule, Some(true));
                    }
                }
                KeyCode::Backspace => drop(text.pop()),
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                _ => {}
            }
            self.tabcfg = Some(ui);
            return;
        }
        let delta = match k.code {
            KeyCode::Left | KeyCode::BackTab => -1,
            KeyCode::Right | KeyCode::Tab | KeyCode::Char(' ') => 1,
            _ => 0,
        };
        match (k.code, &rows[sel]) {
            (KeyCode::Esc, _) => return,
            (KeyCode::Char('c'), _) if k.modifiers.contains(KeyModifiers::CONTROL) => return,
            (KeyCode::Up | KeyCode::Down, _) => ui.sel = panel::cycle(sel, rows.len(), k.code == KeyCode::Up),
            (KeyCode::Enter, Row::Add) | (_, Row::Add) if delta != 0 || k.code == KeyCode::Enter => ui.adding = Some(String::new()),
            (_, Row::Mode) if delta != 0 || k.code == KeyCode::Enter => {
                let modes = tabcfg::modes();
                let cur = modes.iter().position(|m| *m == self.sessions[self.active].mode).map_or(0, |i| i as isize);
                let next = modes[(cur + if delta == 0 { 1 } else { delta }).rem_euclid(modes.len() as isize) as usize];
                self.set_mode(next);
            }
            (KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace, Row::Tool(name, _)) => self.s().set_rule(name, None),
            (_, Row::Tool(name, _)) if delta != 0 || k.code == KeyCode::Enter => {
                let now = self.sessions[self.active].rule(name);
                self.s().set_rule(name, tabcfg::step(now, if delta == 0 { 1 } else { delta }));
            }
            _ => {}
        }
        ui.sel = ui.sel.min(tabcfg::rows(&self.sessions[self.active]).len() - 1);
        self.tabcfg = Some(ui);
    }

    fn set_mode(&mut self, mode: &str) {
        let s = self.s();
        s.control("set_permission_mode", json!({"mode": mode}));
        s.mode = mode.into();
        s.status_dirty = true;
    }

    fn settings_key(&mut self, k: KeyEvent) {
        use settings::Row;
        let Some(mut ui) = self.settings.take() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        ui.note = None;
        if let Some(mut text) = ui.edit.take() {
            match k.code {
                KeyCode::Esc => {}
                KeyCode::Char('c') if ctrl => {}
                KeyCode::Enter => {
                    if let Some(Row::Cc(i)) = ui.at() {
                        let key = &cc_keys::KEYS[i];
                        if let Err(e) = settings::parse(key, &text).and_then(|v| ui.save(key, v)) {
                            ui.note = Some((e, true));
                            ui.edit = Some(text);
                        } else {
                            ui.note = Some((format!("{} saved to {} settings", key.key, ui.scope.name()), false));
                        }
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                    ui.edit = Some(text);
                }
                KeyCode::Char(c) if !ctrl => {
                    text.push(c);
                    ui.edit = Some(text);
                }
                _ => ui.edit = Some(text),
            }
            return self.settings = Some(ui);
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('c') if k.code == KeyCode::Esc || ctrl => {
                if ui.query.is_empty() {
                    return; // closed
                }
                (ui.query, ui.sel) = (String::new(), 0);
            }
            KeyCode::Up => ui.step(-1),
            KeyCode::Down => ui.step(1),
            KeyCode::PageUp => ui.step(-10),
            KeyCode::PageDown => ui.step(10),
            KeyCode::Tab => ui.switch_tab(1),
            KeyCode::BackTab => ui.switch_tab(-1),
            KeyCode::Left => self.settings_change(&mut ui, -1),
            KeyCode::Right | KeyCode::Enter => self.settings_change(&mut ui, 1),
            KeyCode::Char(' ') if ui.query.is_empty() => self.settings_change(&mut ui, 1),
            KeyCode::Char('s') if ctrl => ui.cycle_scope(),
            KeyCode::Delete => {
                if let Some(Row::Cc(i)) = ui.at() {
                    let key = &cc_keys::KEYS[i];
                    ui.note = Some(match ui.save(key, None) {
                        Ok(()) => (format!("{} removed from {} settings", key.key, ui.scope.name()), false),
                        Err(e) => (e, true),
                    });
                }
            }
            KeyCode::Backspace => {
                ui.query.pop();
                ui.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                ui.query.push(c);
                ui.sel = 0;
            }
            _ => {}
        }
        self.settings = Some(ui);
    }

    /// ←→/enter on a `/config` row: step a choice, or start typing a value.
    fn settings_change(&mut self, ui: &mut settings::Ui, delta: isize) {
        match ui.at() {
            Some(settings::Row::Own(i)) => self.change_setting(i, delta),
            Some(settings::Row::Cc(i)) => {
                let key = &cc_keys::KEYS[i];
                match ui.cycled(key, delta) {
                    Some(v) => {
                        ui.note = Some(match ui.save(key, v) {
                            Ok(()) => (format!("{} saved to {} settings", key.key, ui.scope.name()), false),
                            Err(e) => (e, true),
                        })
                    }
                    None => ui.edit = Some(ui.edit_text(key)),
                }
            }
            None => {}
        }
    }

    /// Colours changed: drop every rendered line that baked the old ones in, and repaint the whole screen.
    fn restyle(&mut self) {
        for s in &mut self.sessions {
            s.items.iter_mut().for_each(|i| i.cache = None);
        }
        self.clear_request = true;
    }

    /// What a setting is right now (the live value, which a flag may have set).
    pub fn setting_value(&self, key: &str) -> String {
        match key {
            "statusline" => self.statusline_mode.name().to_string(),
            "profile" => self.default_profile.clone(),
            "images" => image::mode().to_string(),
            "cache_ttl_min" => (self.cache_ttl.as_secs() / 60).to_string(),
            "idle_suspend_s" => self.idle_suspend.as_secs().to_string(),
            "mem_budget_mb" => (self.mem_budget_kb / 1024).to_string(),
            "notify" => self.notify.clone(),
            "mouse" => if self.mouse { "on" } else { "off" }.to_string(),
            "autocopy" => if self.autocopy { "on" } else { "off" }.to_string(),
            "side_panel" => if self.dock_mode { "on" } else { "off" }.to_string(),
            "input_mode" => if self.vim.on { "vim" } else if self.editor_mode { "editor" } else { "default" }.to_string(),
            "theme" => theme::mode_name().to_string(),
            "queue" => if self.queue_on { "on" } else { "off" }.to_string(),
            _ => String::new(),
        }
    }

    fn change_setting(&mut self, index: usize, delta: isize) {
        let item = &settings::ITEMS[index];
        let value = settings::cycle(item, &self.setting_value(item.key), delta);
        self.apply_setting(item.key, value);
    }

    /// Apply a setting now and remember it in the config file.
    fn apply_setting(&mut self, key: &str, value: &str) {
        let num = value.parse::<u64>().unwrap_or(0);
        match key {
            "statusline" => {
                if let Some(note) = self.apply_statusline_mode(statusline::Mode::parse(value).unwrap_or(statusline::Mode::Builtin)) {
                    self.flash(format!("{note}; showing claude++'s own line"));
                }
            }
            "profile" => self.default_profile = value.into(),
            "images" => image::set_mode(value),
            "cache_ttl_min" => {
                self.cache_ttl = Duration::from_secs(num * 60);
                let ttl = self.cache_ttl;
                self.sessions.iter_mut().for_each(|s| s.cache_ttl = ttl);
            }
            "idle_suspend_s" => self.idle_suspend = Duration::from_secs(num),
            "mem_budget_mb" => self.mem_budget_kb = num * 1024,
            "mouse" => self.set_mouse(value != "off"),
            "autocopy" => self.autocopy = value == "on",
            "notify" => self.notify = value.into(),
            "theme" => {
                theme::set_mode(value);
                self.restyle();
            }
            "input_mode" => {
                self.vim.on = value == "vim";
                self.editor_mode = value == "editor";
                self.vim.mode = vim::Mode::Insert;
                self.vim_shape();
            }
            "queue" => self.queue_on = value == "on",
            "side_panel" => {
                self.dock_mode = value == "on";
                if !self.dock_mode {
                    (self.dock, self.dock_focus) = (None, false);
                }
            }
            _ => return,
        }
        if let Some(p) = self.config_path.clone()
            && let Err(e) = config::set_str(&p, key, value)
        {
            self.flash(format!("could not save {}: {e}", p.display()));
        }
    }

    fn open_model_picker(&mut self) {
        if self.model_list().is_empty() {
            // Nothing remembered yet: start the backend so the list arrives.
            let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
            self.s().warm(&launch, &tx);
        }
        let s = &self.sessions[self.active];
        let sel = self.current_model().unwrap_or(0);
        let effort = self.model_list().get(sel).and_then(|m| m.efforts.iter().position(|e| *e == s.effort)).unwrap_or(0);
        let fast = s.fast_mode || s.fast_pref == Some(true);
        self.model_picker = Some(ModelPicker { sel, effort, fast });
    }

    /// The colour the whole UI wears: the highlighted one while `/color` is open, else that of the conversation
    /// being browsed in the session list, else that of the tab in front.
    pub fn accent_color(&mut self) -> Option<String> {
        if let Some(c) = self.picked_color() {
            return c;
        }
        let list = if self.explorer.is_some() { self.explorer.as_mut() } else if self.dock_focus { self.dock.as_mut() } else { None };
        if let Some(list) = list
            && let Some((id, _)) = list.selected()
        {
            return match self.sessions.iter().find(|s| s.session_id.as_deref() == Some(id.as_str())) {
                Some(s) => s.color.clone(),
                None => list.color(),
            };
        }
        self.sessions[self.active].color.clone()
    }

    /// Tab `i`'s colour; the tab in front shows the colour being picked.
    pub fn tab_color(&self, i: usize) -> Option<&str> {
        match self.color_picker.and_then(|p| Self::color_choices().get(p).copied()) {
            Some(c) if i == self.active => Some(c).filter(|c| *c != "default"),
            _ => self.sessions[i].color.as_deref(),
        }
    }

    /// While `/color` is open: the highlighted choice (`None` inside is `default`).
    fn picked_color(&self) -> Option<Option<String>> {
        let c = *Self::color_choices().get(self.color_picker?)?;
        Some((c != "default").then(|| c.to_string()))
    }

    /// The picker's rows: `default`, then each colour.
    pub fn color_choices() -> Vec<&'static str> {
        std::iter::once("default").chain(theme::color_names()).collect()
    }

    fn color_key(&mut self, k: KeyEvent) {
        let Some(sel) = self.color_picker else { return };
        let n = Self::color_choices().len();
        match k.code {
            KeyCode::Esc => self.color_picker = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.color_picker = None,
            KeyCode::Up | KeyCode::Down => self.color_picker = Some(panel::cycle(sel, n, k.code == KeyCode::Up)),
            KeyCode::Enter => {
                self.color_picker = None;
                // Through the normal path, so Claude Code records it with the conversation.
                self.input = format!("/color {}", Self::color_choices()[sel]);
                self.cursor = self.input.chars().count();
                self.submit();
            }
            _ => {}
        }
    }

    fn open_rewind(&mut self) {
        let n = self.s().rewind_points().len();
        if n == 0 {
            return self.flash("nothing to rewind to");
        }
        self.flash = None;
        self.rewind = Some(RewindPicker { sel: n - 1, restore: 0 });
    }

    fn rewind_key(&mut self, k: KeyEvent) {
        let Some(mut p) = self.rewind else { return };
        let points = self.s().rewind_points();
        let n = points.len();
        if n == 0 {
            return self.rewind = None;
        }
        p.sel = p.sel.min(n - 1);
        let choices = RewindPicker::choices(self.s().edits_since(points[p.sel].0));
        match k.code {
            KeyCode::Esc => return self.rewind = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return self.rewind = None,
            KeyCode::Up | KeyCode::Down => p.sel = panel::cycle(p.sel, n, k.code == KeyCode::Up),
            KeyCode::Left => p.restore = p.restore.saturating_sub(1),
            KeyCode::Right => p.restore += 1,
            KeyCode::Enter => {
                self.rewind = None;
                let (_, conversation, code) = choices[p.restore.min(choices.len() - 1)];
                let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
                match self.s().rewind(points[p.sel].0, conversation, code, &launch, &tx) {
                    Ok(Some(text)) => self.set_input(text),
                    Ok(None) => {}
                    Err(e) => self.flash(&e),
                }
                return;
            }
            _ => return,
        }
        // Each message offers its own choices; keep the cursor on one that exists.
        p.restore = p.restore.min(RewindPicker::choices(self.s().edits_since(points[p.sel].0)).len() - 1);
        self.rewind = Some(p);
    }

    fn picker_key(&mut self, k: KeyEvent) {
        let n = self.model_list().len();
        let Some(mut p) = self.model_picker else { return };
        // The list can shrink under an open picker (the backend's own list replaces the cached one).
        p.sel = p.sel.min(n.saturating_sub(1));
        let levels = |sel: usize| self.model_list().get(sel).map_or(0, |m| m.efforts.len());
        let next = match k.code {
            KeyCode::Esc => return self.model_picker = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return self.model_picker = None,
            KeyCode::Enter => return self.apply_model_picker(),
            KeyCode::Up | KeyCode::Down if n > 0 => {
                let new = panel::cycle(p.sel, n, k.code == KeyCode::Up);
                // Keep the same level name when the next model offers it.
                let name = self.model_list()[p.sel].efforts.get(p.effort).cloned();
                let keep = name.and_then(|e| self.model_list()[new].efforts.iter().position(|x| *x == e));
                ModelPicker { sel: new, effort: keep.unwrap_or(p.effort.min(levels(new).saturating_sub(1))), ..p }
            }
            KeyCode::Left => ModelPicker { effort: p.effort.saturating_sub(1), ..p },
            KeyCode::Right => ModelPicker { effort: (p.effort + 1).min(levels(p.sel).saturating_sub(1)), ..p },
            // Space (as in Claude Code's /fast) or f toggles fast mode, for models that have it.
            KeyCode::Char(' ' | 'f') if self.model_list().get(p.sel).is_some_and(|m| m.fast) => ModelPicker { fast: !p.fast, ..p },
            _ => return,
        };
        self.model_picker = Some(next);
    }

    fn apply_model_picker(&mut self) {
        let Some(p) = self.model_picker.take() else { return };
        let Some(m) = self.model_list().get(p.sel).cloned() else { return };
        let level = m.efforts.get(p.effort).cloned();
        let model_changed = self.current_model() != Some(p.sel);
        let s = &self.sessions[self.active];
        let wants_fast = p.fast && m.fast;
        let had_fast = s.fast_mode || s.fast_pref == Some(true);
        let effort_changed = level.as_ref().is_some_and(|l| *l != s.effort);
        let fast_changed = wants_fast != had_fast;
        if !model_changed && !effort_changed && !fast_changed {
            return;
        }
        let warn = if model_changed { self.s().recache_warning() } else { String::new() };
        let k = self.s().ctx_tokens / 1000;
        if model_changed {
            self.s().choose_model(&m.value);
        }
        let mut what = vec![m.name.clone()];
        if let Some(l) = level.filter(|_| model_changed || effort_changed) {
            self.s().choose_effort(&l);
            what.push(format!("effort {l}"));
        }
        // Turning it on (or keeping it on across a model switch) is sent; turning it off is sent unless
        // choose_model already did so because the new model lacks fast mode.
        if wants_fast && (fast_changed || model_changed) {
            self.s().choose_fast(true);
        } else if !wants_fast && had_fast && self.s().fast_pref != Some(false) {
            self.s().choose_fast(false);
        }
        if wants_fast && fast_changed {
            what.push("fast ↯".into());
            let cost = if k > 0 { format!(" The first fast request re-reads the whole conversation (~{k}k tokens) at fast pricing, once.") } else { String::new() };
            self.s().note(format!("{}. Fast mode costs more per token and is billed from usage credits.{cost}{warn}", what.join(" · ")));
        } else {
            self.s().note(format!("{}.{warn}", what.join(" · ")));
        }
    }

    fn submit(&mut self) {
        if self.sessions[self.active].rewinding {
            return self.flash("restoring files; send once they are back");
        }
        self.reset_undo = true;
        if self.vim.on {
            self.vim.mode = vim::Mode::Insert; // sent: type the next one
        }
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_pos = None;
        if text.trim().is_empty() && self.s().attachments.is_empty() {
            return;
        }
        // Recalled text cannot bring a pasted picture back, so history keeps the words only.
        let words = image::strip_tokens(&text);
        if !words.trim().is_empty() {
            let sid = self.sessions[self.active].session_id.clone();
            history::record_prompt(self.cwd(), &words, sid.as_deref());
            if self.history.last() != Some(&words) {
                self.history.push(words);
            }
        }
        // A time was picked with /schedule: this message waits for it.
        if let Some(due) = self.sched_at.take() {
            return self.queue_message(text, Some(due));
        }
        // `!command` runs here, as in Claude Code: no model turn, its output rides along with the next message.
        if let Some(cmd) = text.trim().strip_prefix('!').map(str::trim).filter(|c| !c.is_empty()) {
            let (cmd, sid, tx) = (cmd.to_string(), self.sessions[self.active].id, self.tx.clone());
            let dir = self.sessions[self.active].cwd.clone().unwrap_or_else(|| self.cwd.clone());
            let id = self.s().start_shell(&cmd);
            // ponytail: no timeout or cancel; a command that never ends just never reports (the UI stays free).
            std::thread::spawn(move || {
                let out = std::process::Command::new("sh").args(["-c", &cmd]).current_dir(dir).stdin(std::process::Stdio::null()).output();
                let _ = tx.send(AppEvent::Shell(sid, id, cmd, out));
            });
            return;
        }
        let (cmd, arg) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
        let arg = arg.trim().to_string();
        match cmd {
            "/quit" | "/exit" => self.quit = true,
            // In the app only the keys and commands matter; the flags are for the command line.
            "/help" => self.s().note(HELP[HELP.find("KEYS").unwrap_or(0)..].trim_end().into()),
            "/new" => {
                if !arg.is_empty()
                    && let Err(e) = profile_args(&arg)
                {
                    return self.flash(e);
                }
                self.new_tab();
                if !arg.is_empty() {
                    self.s().profile = arg;
                }
            }
            "/profile" if arg.is_empty() => {
                let cur = self.s().profile.clone();
                let all = ["full", "sources", "lean"].map(String::from).into_iter().chain(profile::names()).collect::<Vec<_>>().join(" | ");
                let dir = profile::dir().map(|d| d.display().to_string()).unwrap_or_default();
                self.s().note(format!("profile: {cur}. Available: {all}. /profile <name> changes it for this tab; /new <name> opens a tab with one. Your own live in {dir}/<name>.json"));
            }
            "/profile" => {
                let (name, hard) = match arg.strip_suffix(" --hard") {
                    Some(n) => (n.trim().to_string(), true),
                    None => (arg.clone(), false),
                };
                if let Err(e) = profile_args(&name) {
                    return self.flash(e);
                }
                if self.s().profile == name && !hard {
                    return;
                }
                if let Err(e) = self.s().restart_later() {
                    return self.flash(e);
                }
                let started = self.s().ctx_tokens > 0;
                let k = self.s().ctx_tokens / 1000;
                self.s().profile = name.clone();
                let msg = if hard {
                    self.s().unpinned = true;
                    format!("profile → {name} with a re-rendered prompt; the next message re-caches up to ~{k}k tokens")
                } else if started {
                    format!(
                        "profile → {name}: tool/MCP changes apply on your next message. This conversation keeps the system prompt and settings it started with (Claude Code pins them); `/profile {name} --hard` re-renders them (re-caches up to ~{k}k tokens), `/new {name}` starts clean"
                    )
                } else {
                    format!("profile → {name}")
                };
                self.s().note(msg);
            }
            "/resume" if arg.is_empty() => self.open_explorer(),
            "/sessions" => self.toggle_sessions(),
            "/resume" => {
                // An id, or the start of one; the explorer is for when you do not know it.
                // Also a conversation's name, and from any folder, as `claude --resume` takes them.
                let hit = |f: &history::Summary| f.id.starts_with(&arg) || f.title.eq_ignore_ascii_case(&arg);
                let mut here: Vec<_> = history::recent(self.cwd(), 500).into_iter().filter(|f| hit(f)).map(|f| (f, None)).collect();
                if here.is_empty() {
                    here = history::recent_all(2000).into_iter().filter(|f| hit(f)).map(|f| { let d = Some(f.cwd.clone()).filter(|d| !d.is_empty()); (f, d) }).collect();
                }
                if here.len() > 1 && !here.iter().any(|(f, _)| f.id == arg) {
                    let n = here.len();
                    self.s().note(format!("{n} sessions match {arg:?}: type more of the id, or /resume to pick one"));
                    return;
                }
                let found = here.into_iter().next();
                match (found, history::find(&arg)) {
                    (Some((f, dir)), _) => self.open_session(f.id, f.title, dir),
                    (None, Some(_)) => self.open_session(arg, String::new(), None),
                    (None, None) => self.s().note(format!("no session with an id starting {arg:?} or named so; /resume lists them")),
                }
            }
            "/statusline" => match statusline::Mode::parse(&arg).filter(|m| !matches!(m, statusline::Mode::Command(_))) {
                None if arg.is_empty() => {
                    let claude = statusline::lookup(std::path::Path::new(self.cwd()), &self.claude_paths).found;
                    let theirs = claude.map_or("none found in your Claude Code settings".to_string(), |c| format!("`{}` from {:?} settings", c.command, c.source));
                    let now = match &self.statusline_mode {
                        statusline::Mode::Builtin => "builtin: claude++'s own line only, nothing is spawned".to_string(),
                        statusline::Mode::Claude => format!("claude: running {theirs}"),
                        statusline::Mode::Command(c) => format!("command (from --statusline, not saved): `{c}`"),
                    };
                    self.s().note(format!("status line → {now}\nClaude Code's would be: {theirs}\n/statusline claude | builtin switches and remembers the choice"));
                }
                None => self.flash("usage: /statusline [claude|builtin]"),
                Some(mode) => {
                    let name = mode.name().to_string();
                    let note = self.apply_statusline_mode(mode);
                    let saved = match self.config_path.clone() {
                        Some(p) => match config::set_str(&p, "statusline", &name) {
                            Ok(()) => format!("remembered in {}", p.display()),
                            Err(e) => format!("could not save to {}: {e}", p.display()),
                        },
                        None => "not saved (no config location)".into(),
                    };
                    let extra = note.map(|n| format!(". {n}, so claude++'s own line is shown")).unwrap_or_default();
                    self.s().note(format!("status line → {name}{extra} ({saved})"));
                }
            },
            "/config" | "/setting" => self.settings = Some(settings::Ui::new(self.cwd())),
            "/session" => self.tabcfg = Some(tabcfg::Ui::default()),
            "/optimize" => self.optimize_cmd(&arg),
            "/permissions" => self.perms = Some(permissions::Ui::new(self.cwd(), &self.sessions[self.active])),
            "/instructions" => self.instr = Some(instructions::Ui::new(self.cwd(), instructions::rules_path().map(|f| instructions::load_rules(&f)).unwrap_or_default())),
            "/usage" | "/cost" | "/status" => self.open_usage(),
            "/context" => {
                self.ctx_ui = Some(Default::default());
                self.ask_usage();
            }
            "/mode" if arg.is_empty() => self.tabcfg = Some(tabcfg::Ui::default()),
            "/memory" => {
                let dir = if arg == "user" { config::claude_dir() } else { Some(self.cwd().into()) };
                self.edit_file = dir.map(|d| d.join("CLAUDE.md"));
            }
            "/export" => self.export(&arg),
            "/tasks" => self.lanes = Some(Default::default()),
            "/queue" => self.queue_ui = Some(Default::default()),
            "/schedule" => {
                // `10m do this` schedules at once; `2h` waits for the message to be typed; anything else asks for the time.
                let arg = arg.trim();
                let (when, msg) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
                let (msg, due) = (msg.trim(), queue::parse_when(when, SystemTime::now()));
                match due {
                    Some(due) if !msg.is_empty() => self.queue_message(msg.to_string(), Some(due)),
                    Some(due) => {
                        self.sched_at = Some(due);
                        self.flash("now type the message: enter schedules it, esc cancels");
                    }
                    None => {
                        let mut ui = schedule::Ui::new(when);
                        ui.msg = Some(msg.to_string()).filter(|m| !m.is_empty());
                        self.sched_ui = Some(ui);
                    }
                }
            }
            "/search" => {
                self.open_search();
                if !arg.is_empty() && let Some(ui) = self.search.as_mut() {
                    ui.query = arg;
                    ui.refresh(&self.sessions[self.active].items, &self.history);
                    let at = ui.reveal();
                    self.s().reveal = at;
                }
            }
            "/editor" => {
                self.apply_setting("input_mode", if self.editor_mode { "default" } else { "editor" });
                self.flash(if self.editor_mode { "editor mode on: the message area is your editor, quit it to send; ctrl+\\ cancels; /editor turns it off" } else { "editor mode off" });
            }
            "/vim" => {
                self.apply_setting("input_mode", if self.vim.on { "default" } else { "vim" });
                self.flash(if self.vim.on { "vim keys on: esc for normal mode; /vim turns them off" } else { "vim keys off" });
            }
            "/close" => self.close_tab_asking(),
            "/clear" => self.s().clear(),
            "/copy" => self.copy_last(arg.as_str()),
            "/find" => {
                let s = self.s();
                let hit = match arg.as_str() {
                    "" if s.find_query.is_empty() => return self.flash("usage: /find <text>; /find again steps to the next older match, /find + to a newer one"),
                    "" => s.find_step(true),
                    "+" => s.find_step(false),
                    q => s.find(q),
                };
                let q = s.find_query.clone();
                match hit {
                    Some((k, n)) => self.flash(format!("{q:?}: match {k} of {n} (newest first); /find steps older, /find + newer")),
                    None => self.flash(format!("{q:?}: no match in this conversation")),
                }
            }
            "/img" if arg.is_empty() => self.flash("usage: /img <path to png/jpeg/gif/webp>"),
            "/img" => self.attach(arg.trim_matches(|c| c == '\'' || c == '"')),
            "/model" | "/effort" if arg.is_empty() => self.open_model_picker(),
            "/color" if arg.is_empty() => {
                let cur = self.sessions[self.active].color.as_deref().unwrap_or("default");
                self.color_picker = Self::color_choices().iter().position(|c| *c == cur);
            }
            "/model" => {
                let warn = self.s().recache_warning();
                self.s().choose_model(&arg);
                self.s().note(format!("model → {arg}.{warn}"));
            }
            "/effort" => {
                let known = self.model_list().iter().find(|m| self.sessions[self.active].model == m.resolved).map(|m| m.efforts.clone());
                match known.filter(|e| !e.is_empty()) {
                    Some(levels) if !levels.contains(&arg) => self.s().note(format!("effort: this model supports {}", levels.join(", "))),
                    _ => {
                        self.s().choose_effort(&arg);
                        self.s().note(format!("effort → {arg}"));
                    }
                }
            }
            "/mode" if !arg.is_empty() => {
                self.s().control("set_permission_mode", json!({"mode": arg}));
                self.s().mode = arg;
                self.s().status_dirty = true;
            }
            _ => {
                if cmd == "/color" {
                    self.s().set_color(&arg);
                }
                let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
                let mentioned = self.mentioned_images(&text);
                // Claude is working (or messages already wait): this one waits its turn, managed with alt+q.
                if !text.starts_with('/') && ((self.queue_on && self.s().busy) || self.s().queue.iter().any(|q| q.due.is_none())) {
                    self.queue_message(text, None);
                    return self.drain_queues();
                }
                self.undo.clear(); // sent: what was only kept for undo goes now
                self.redo.clear();
                self.drop_orphan_images(&text);
                self.s().submit(text, &mentioned, &launch, &tx);
            }
        }
    }
}

fn cache_path(name: &str) -> Option<std::path::PathBuf> {
    config::cache_dir().map(|d| d.join(name))
}

fn commands_cache() -> Option<std::path::PathBuf> {
    cache_path("commands.json")
}

fn read_models(path: &std::path::Path) -> Vec<ModelInfo> {
    let list: Option<Vec<serde_json::Value>> = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok());
    let s = |c: &serde_json::Value, k: &str| c[k].as_str().unwrap_or_default().to_string();
    list.unwrap_or_default()
        .iter()
        .map(|m| ModelInfo {
            value: s(m, "value"),
            name: s(m, "name"),
            desc: s(m, "desc"),
            resolved: s(m, "resolved"),
            efforts: m["efforts"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect(),
            fast: m["fast"] == true,
        })
        .collect()
}

fn write_models(p: &std::path::Path, models: &[ModelInfo]) {
    let v: Vec<_> = models
        .iter()
        .map(|m| json!({"value": m.value, "name": m.name, "desc": m.desc, "resolved": m.resolved, "efforts": m.efforts, "fast": m.fast}))
        .collect();
    let _ = p.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(p, serde_json::Value::Array(v).to_string());
}

fn load_commands() -> Vec<Cmd> {
    commands_cache().map(|p| read_commands(&p)).unwrap_or_default()
}

fn read_commands(path: &std::path::Path) -> Vec<Cmd> {
    let list: Option<Vec<serde_json::Value>> =
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok());
    let s = |c: &serde_json::Value, k: &str| c[k].as_str().unwrap_or_default().to_string();
    list.unwrap_or_default().iter().map(|c| Cmd { name: s(c, "name"), desc: s(c, "desc"), hint: s(c, "hint") }).collect()
}

fn save_commands(cmds: &[Cmd]) {
    if let Some(p) = commands_cache() {
        write_commands(&p, cmds);
    }
}

fn write_commands(p: &std::path::Path, cmds: &[Cmd]) {
    let v: Vec<_> = cmds.iter().map(|c| json!({"name": c.name, "desc": c.desc, "hint": c.hint})).collect();
    let _ = p.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(p, serde_json::Value::Array(v).to_string());
}

/// Footprint (KB) per root pid including its child processes. macOS `footprint` reports what the
/// process really holds (RSS overstated it ~2x in our measurements); elsewhere falls back to RSS.
fn tree_mem_kb(roots: &[u32]) -> Vec<(u32, u64)> {
    let ps = std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output();
    let mut kids: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    for l in String::from_utf8_lossy(&ps.map(|o| o.stdout).unwrap_or_default()).lines() {
        let mut it = l.split_whitespace().filter_map(|x| x.parse::<u32>().ok());
        if let (Some(p), Some(pp)) = (it.next(), it.next()) {
            kids.entry(pp).or_default().push(p);
        }
    }
    let trees: Vec<(u32, Vec<u32>)> = roots
        .iter()
        .map(|&r| {
            let (mut all, mut stack) = (vec![], vec![r]);
            while let Some(p) = stack.pop() {
                all.push(p);
                stack.extend(kids.get(&p).into_iter().flatten());
            }
            (r, all)
        })
        .collect();
    let all: Vec<u32> = trees.iter().flat_map(|t| t.1.iter().copied()).collect();
    let mut per_pid = footprint_kb(&all);
    if per_pid.is_empty() {
        per_pid = rss_kb(&all);
    }
    trees.into_iter().map(|(r, ps)| (r, ps.iter().filter_map(|p| per_pid.iter().find(|x| x.0 == *p)).map(|x| x.1).sum())).collect()
}

fn footprint_kb(pids: &[u32]) -> Vec<(u32, u64)> {
    let args: Vec<String> = pids.iter().flat_map(|p| ["-p".to_string(), p.to_string()]).collect();
    match std::process::Command::new("footprint").args(&args).output() {
        Ok(o) => parse_footprint(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => vec![],
    }
}

/// Lines look like `claude [1234]: 64-bit    Footprint: 245 MB (16384 bytes per page)`.
fn parse_footprint(out: &str) -> Vec<(u32, u64)> {
    out.lines()
        .filter_map(|l| {
            let pid = l.split_once('[')?.1.split_once(']')?.0.parse().ok()?;
            let mut it = l.split_once("Footprint:")?.1.split_whitespace();
            let n: f64 = it.next()?.parse().ok()?;
            let mul = match it.next()? {
                "KB" => 1.0,
                "MB" => 1024.0,
                "GB" => 1024.0 * 1024.0,
                _ => return None,
            };
            Some((pid, (n * mul) as u64))
        })
        .collect()
}

/// Resident memory per pid, via `ps` (portable across macOS/Linux, no extra deps).
fn rss_kb(pids: &[u32]) -> Vec<(u32, u64)> {
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let Ok(out) = std::process::Command::new("ps").args(["-o", "pid=,rss=", "-p", &list]).output() else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests;
