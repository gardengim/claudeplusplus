mod backend;
mod cache;
mod cli;
mod tty;
mod input;
mod keys;
mod memory;
mod tabs;
mod btw;
mod config;
mod explorer;
mod jobs;
mod lanes;
mod limits;
mod mcp;
mod motion;
mod screen;
mod segments;
mod shell;
mod swarm;
mod supui;
mod sync;
mod term;
mod termui;
mod share;
mod trust;
mod version;
mod worktree;
mod commands;
mod open;
mod pane_host;
mod panels;
use panels::Panel;
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
mod procs;
mod supervisor;
mod prompt;
mod queue;
mod schedule;
mod search;
mod vim;
mod session;
mod context;
mod settings;
mod statusline;
mod theme;
mod ui;

use backend::{Backend, Launch, Msg};
use cli::{HELP, Opts, parse_args, profile_args, project_profile};
use commands::{Completion, LOCAL_COMMANDS};
use tty::{ENHANCED, Term, alt_scroll_off, edit_file_in_editor, edit_in_editor, editor_command, present, restore_terminal};
use cache::{cache_path, read_commands, read_models, write_commands, write_models};
use memory::spare_hint_once_a_day;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use serde_json::json;
use session::{Choice, Cmd, Entry, ModelInfo, Session};
use std::path::PathBuf;
use std::io::{self, Write};
use std::sync::Arc;
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
    /// The terminal's background turned light (true) or dark while running.
    Theme(bool),
    /// A `/worktree` add or remove finished: what to say and the folder to open in a tab, or why not.
    Worktree(Result<(String, Option<String>), String>),
    /// A terminal of this tab (see `term.rs`) has new output or ended: redraw.
    TermOut(u64),
    /// The session lists were read off the UI thread.
    Sessions((Vec<history::Summary>, Vec<jobs::Job>)),
    /// The messages you sent in this folder, read off the UI thread.
    Sent(Vec<String>),
    /// `/procs` read the process list off the UI thread.
    Procs(Option<procs::Report>),
    /// A one-line note for the first tab, found off the UI thread at start-up.
    Hint(String),
    /// The supervisor connection is up (or could not be made). See `supervisor.rs`.
    Supervisor(Option<Arc<supervisor::Client>>),
    /// A message from the supervisor: the list of claude++ instances, or a tab to stop for the memory budget.
    Sup(supervisor::Msg),
    /// A `/btw` answered: tab id, question number, and the answer and its price or why not.
    Btw(u64, u64, Result<(String, f64), String>),
    /// A resumed tab's transcript was read off the UI thread: tab id, which read, the file, what it holds.
    Resumed(u64, u64, std::path::PathBuf, Box<history::Loaded>),
}

use tabcfg::Row;

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
    idle_suspend: Duration,
    mem_budget_kb: u64,
    pub explorer: Option<explorer::Explorer>,
    /// Config "sessions = panel": ← opens a docked list first, a second ← the full screen.
    pub dock_mode: bool,
    /// `--profile` was given: a project's own profile does not override it.
    profile_forced: bool,
    /// off | bell | desktop: how a background tab gets your attention.
    notify: String,
    /// What `notify` emitted, kept for tests.
    #[cfg(test)]
    sent_notices: Vec<String>,
    /// Real terminal attached: only then are terminal escapes written.
    tty: bool,
    /// The title last written to the terminal.
    term_title: String,
    /// Mouse reporting is on: the wheel scrolls the conversation and a drag selects text.
    mouse: bool,
    /// The selected text: where the drag started and where it ended (screen cells). It stays until the next
    /// key, click or scroll.
    drag: Option<((u16, u16), (u16, u16))>,
    /// The button is still down.
    dragging: bool,
    /// The pane the drag started in, and the panes the last frame drew.
    region: ratatui::layout::Rect,
    panes: Vec<ratatui::layout::Rect>,
    /// The screen's cells as last drawn while a selection stood, for words and the copy.
    snap: Vec<Vec<String>>,
    /// The last press: when, where, and how many in a row (double click = word, triple = line).
    clicks: (Instant, (u16, u16), u8),
    /// Where a selection in the message box began (shift+arrows, or the mouse); the cursor is the other end.
    anchor: Option<usize>,
    /// The message box's text cells as last drawn, and how many rows it was scrolled: a click lands on a character.
    input_rect: ratatui::layout::Rect,
    input_skip: usize,
    /// The button is down in the message box.
    box_live: bool,
    /// What the three kinds of selection (conversation, message box, panels) want copied; `flush_selection` copies it.
    pending_copy: Option<String>,
    copied: String,
    copied_at: Option<Instant>,
    /// The screen-cell selection moved: the draw reads its text.
    sel_dirty: bool,
    pub dock: Option<explorer::Explorer>,
    pub dock_focus: bool,
    /// The session lists as last read (kept so a list opens at once), and whether a read is under way.
    list_cache: Option<(Vec<history::Summary>, Vec<jobs::Job>)>,
    list_loading: bool,
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
    /// `/statusline items`: which parts of our own status line show, and in what order.
    pub items_ui: Option<segments::Ui>,
    /// `/open`: the links and files of this conversation.
    pub open_ui: Option<open::Ui>,
    /// `/worktree`: the repository's worktrees.
    pub wt_ui: Option<worktree::Ui>,
    /// The installed Claude Code that new and restarted tabs run instead of the `claude` on PATH (`/version`).
    pin: Option<version::Installed>,
    /// `!command` runs in your login shell (`$SHELL`) rather than `sh`.
    shell_login: bool,
    /// Where installed Claude Code versions are kept (a field so tests can point it at a folder of their own).
    versions_dir: Option<std::path::PathBuf>,
    pub line_items: Vec<&'static str>,
    pub tabcfg: Option<tabcfg::Ui>,
    pub instr: Option<instructions::Ui>,
    pub perms: Option<permissions::Ui>,
    pub opt: Option<optimize::Ui>,
    pub usage_open: bool,
    /// Numbers the `/btw` questions.
    pub btw_seq: u64,
    /// `/tasks`: the tab's subagents.
    pub lanes: Option<lanes::Ui>,
    pub procs_ui: Option<procs::Ui>,
    /// The connection to the supervisor, once made. Without it nothing else changes.
    sup: Option<Arc<supervisor::Client>>,
    sup_connecting: bool,
    sup_tried: Instant,
    /// The other claude++ instances (and this one), as the supervisor reports them.
    pub peers: Vec<supervisor::Peer>,
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
    pub sup_ui: Option<supui::Ui>,
    /// Where the supervisor lives (the user's own directory; a test points it elsewhere).
    pub sup_paths: supervisor::Paths,
    /// Percent of a plan window at which to warn / at which to hold new messages until it resets; 0 is off.
    pub limit_warn: u8,
    pub limit_hold: u8,
    /// Windows already warned about (name, reset), so each warns once.
    pub limit_warned: Vec<(&'static str, i64)>,
    /// The reset time of the window the user chose to send through anyway.
    pub limit_released: Option<i64>,
    /// Tabs share one copy of each stdio MCP server (`share_mcp`, see `share.rs`).
    pub share_mcp: bool,
    /// Claude gets terminals to drive (`term_open`, `term_send`, ...), hosted by claude++ (see `term.rs`).
    pub agent_term: bool,
    /// `/term`: the list of this tab's terminals, and the one on screen.
    pub term_ui: Option<termui::Ui>,
    pub term_view: Option<std::sync::Arc<term::Term>>,
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
    /// Pass `--exclude-dynamic-system-prompt-sections`: cwd, OS and git status go in the first message instead of
    /// the system prompt, so a new session reuses more of the cached prompt. `"stable_prompt": "off"` in config.json.
    stable_prompt: bool,
    /// `--autocompact <n>k` for backends started from now (0: Claude Code's own, about 967k).
    autocompact_k: u64,
    /// Send `/compact` to an idle tab at least this many k tokens big just before its prompt cache expires (0: never).
    precompact_k: u64,
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
    /// Whether the terminal window has focus, as the terminal reports it (`None`: it never has, so it cannot be told),
    /// and since when.
    focus: Option<bool>,
    focus_at: Instant,
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
    /// Two menus, one with the keys: the messages (up and down) or what to restore (left and right).
    pub on_restore: bool,
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
    /// The one menu with the keys: 0 models, 1 versions, 2 effort, 3 fast. Tab moves it.
    pub focus: u8,
}

impl ModelPicker {
    /// Which menus can have the keys for the highlighted model: the models always, the versions when the family
    /// has several, the effort when the model has levels, fast mode when the model has it.
    pub fn usable(models: &[session::ModelInfo], sel: usize) -> [bool; 4] {
        let fams = session::families(models);
        let m = models.get(sel);
        [true, fams.iter().any(|f| f.1.len() > 1 && f.1.contains(&sel)), m.is_some_and(|m| !m.efforts.is_empty()), m.is_some_and(|m| m.fast)]
    }

    /// The menu with the keys now: the one chosen, or the models when that one cannot be used here.
    pub fn focused(&self, usable: [bool; 4]) -> usize {
        let f = self.focus as usize % 4;
        if usable[f] { f } else { 0 }
    }
}
















fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("procs") => return procs::print(),
        Some("supervisor") => {
            let ended = supervisor::run(&supervisor::Paths::user(), &supervisor::Config::user());
            share::stop_all(); // its shared MCP servers must not outlive it
            if let Err(e) = ended {
                eprintln!("claude++ supervisor: {e}");
                std::process::exit(1);
            }
            return;
        }
        _ => {}
    }
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

/// A new `App` with every setting applied: a flag wins for this run, else what /config saved, else the built-in default.
/// Also returns the note about a pinned Claude Code version that is gone, for the first tab.
fn configure(opts: &Opts, tx: &Sender<AppEvent>, cwd: String) -> (App, Option<String>) {
    // Every setting: a flag wins for this run, else what /config saved, else the built-in default.
    let config_path = config::path();
    let saved = |key: &str| config_path.as_deref().and_then(|p| config::get_str(p, key));
    let saved_num = |key: &str| saved(key).and_then(|v| v.parse::<u64>().ok());
    let idle = opts.idle.or(saved_num("idle_suspend_s")).unwrap_or(120);
    let mut app = App::new(opts.launch.clone(), cwd, tx.clone(), Duration::from_secs(idle));
    app.config_path = config_path.clone();
    app.mem_budget_kb = opts.mem_budget_mb.or(saved_num("mem_budget_mb")).unwrap_or(0) * 1024;
    app.profile_forced = !opts.profile.is_empty();
    let profile = Some(opts.profile.clone()).filter(|p| !p.is_empty()).or_else(|| saved("profile").filter(|p| profile_args(p).is_ok()));
    if let Some(p) = profile {
        app.default_profile = p;
    }
    app.cache_ttl = Duration::from_secs(opts.cache_ttl_min.or(saved_num("cache_ttl_min")).unwrap_or(60) * 60);
    app.autocompact_k = saved_num("autocompact_k").filter(|k| (100..=1000).contains(k)).unwrap_or(0);
    app.precompact_k = saved_num("precompact_k").filter(|k| (50..=1000).contains(k)).unwrap_or(0);
    let mut pin_note: Option<String> = None;
    if let Some(v) = saved("claude_version").filter(|v| !v.is_empty()) {
        match app.versions_dir.as_deref().map(version::installed_in).unwrap_or_default().into_iter().find(|i| i.version == v) {
            _ if app.launch.bin != "claude" => {} // --claude-bin names the executable
            Some(i) => app.pin = Some(i),
            None => pin_note = Some(format!("Claude Code {v} (pinned with /version) is not installed any more; running the default. /version lists what is")),
        }
    }
    app.shell_login = saved("shell").as_deref() != Some("sh");
    if let Some(list) = saved("statusline_items") {
        app.line_items = segments::parse(&list);
    }
    app.stable_prompt = saved("stable_prompt").as_deref() != Some("off");
    image::set_mode(&saved("images").unwrap_or_default());
    motion::configure(&saved("spinner").unwrap_or_default(), &saved("progress").unwrap_or_default(), &saved("motion").unwrap_or_default());
    if let Some(p) = config_path.as_deref() {
        let folds = config::get_value(p, "session_folds").and_then(|v| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()));
        let group = saved("session_group").and_then(|g| g.parse().ok()).unwrap_or(0);
        explorer::set_group(group);
        app.list_prefs = (explorer::group(), folds.unwrap_or_default());
    }
    app.dock_mode = saved("side_panel").as_deref() == Some("on");
    let mode = saved("input_mode").unwrap_or_default();
    app.vim.on = mode == "vim";
    app.editor_mode = mode == "editor";
    app.editor_cmd = saved("editor").filter(|c| !c.trim().is_empty());
    app.queue_on = saved("queue").as_deref() != Some("off");
    if let Some(n) = saved_num("limit_warn").filter(|n| *n <= 100) {
        app.limit_warn = n as u8;
    }
    if let Some(n) = saved_num("limit_hold").filter(|n| *n <= 100) {
        app.limit_hold = n as u8;
    }
    app.share_mcp = saved("share_mcp").as_deref() == Some("on");
    app.agent_term = saved("agent_term").as_deref() == Some("on");
    if let Some(n) = saved("notify").filter(|n| ["off", "bell", "desktop"].contains(&n.as_str())) {
        app.notify = n;
    }
    let mode = statusline::Mode::parse(&opts.statusline).or_else(|| saved("statusline").as_deref().and_then(statusline::Mode::parse));
    app.startup_note = app.apply_statusline_mode(mode.unwrap_or(statusline::Mode::Builtin));
    (app, pin_note)
}

fn run(opts: Opts) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let (mut app, pin_note) = configure(&opts, &tx, cwd);
    let config_path = app.config_path.clone();
    let saved = |key: &str| config_path.as_deref().and_then(|p| config::get_str(p, key));
    app.known_commands = cache_path("commands.json").map(|p| read_commands(&p)).unwrap_or_default();
    spare_hint_once_a_day(&tx);
    {
        let (tx, cwd) = (tx.clone(), app.cwd.clone());
        thread::spawn(move || {
            let _ = tx.send(AppEvent::Sent(history::sent_prompts(&cwd)));
        });
    }
    app.load_sessions(); // so the first ← finds the list read
    app.known_models = cache_path("models.json").map(|p| read_models(&p)).unwrap_or_default();
    app.new_tab();
    for n in app.startup_note.take().into_iter().chain(pin_note) {
        app.sessions[0].note(n);
    }
    if let Some(id) = opts.resume {
        app.sessions[0].resume(id, &tx);
    } else if opts.cont {
        let cwd = app.cwd.clone();
        app.sessions[0].continue_last(&cwd, &tx);
    }

    if let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP]) {
        let tx = tx.clone();
        thread::spawn(move || {
            for _ in signals.forever() {
                let _ = tx.send(AppEvent::Quit);
            }
        });
    }
    // What `ratatui::init()` does, with the frame-at-a-time output (see screen.rs) and without its panic hook.
    let _ = terminal::enable_raw_mode();
    let _ = execute!(io::stdout(), terminal::EnterAlternateScreen);
    let frames = screen::Frames::new(io::stdout());
    let mut term: Term = (ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(frames.clone()))?, frames);
    theme::set_mode(saved("theme").as_deref().unwrap_or("auto"));
    if let Some(light) = theme::query_light_background() {
        theme::set_light(light);
    }
    app.tty = true;
    let mut out = io::stdout();
    let _ = execute!(out, EnableBracketedPaste, EnableFocusChange);
    alt_scroll_off();
    app.mouse = saved("mouse").as_deref() != Some("off");
    if app.mouse {
        app.set_mouse(true);
    }
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
    tty::spawn_reader(tx);

    let res = (|| -> io::Result<()> {
        while !app.quit {
            app.sync_term_watch();
            motion::advance(app.sessions.iter().any(|s| s.busy));
            term.0.draw(|f| ui::draw(f, &mut app))?;
            present(&term);
            app.sync_title();
            // Busy: wake just after the next spinner frame is due, so every frame is shown for the same time
            // (a fixed wait drifts against the animation and repeats one frame, skips the next: it shivers).
            let look = motion::look();
            let wait = match app.sessions.get(app.active) {
                Some(s) if s.busy => look.wait_ms(s.busy_since.elapsed()),
                _ if app.sessions.iter().any(|s| s.busy) => look.background_wait_ms(),
                _ => 1000,
            };
            // A pointer held past the edge of the conversation keeps scrolling it.
            let wait = if app.sessions.get(app.active).is_some_and(|s| s.sel.live && s.sel.edge != 0) { wait.min(50) } else { wait };
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
            app.autoscroll_selection();
            app.tick();
            if std::mem::take(&mut app.clear_request) {
                let _ = term.0.clear(); // ctrl+l: repaint everything, e.g. after another program wrote to the screen
                present(&term);
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
    // A clean exit: the supervisor must not treat the processes closed below as left behind.
    share::set_hub(None);
    if let Some(c) = app.sup.take() {
        c.bye();
    }
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
            idle_suspend,
            mem_budget_kb: 0,
            explorer: None,
            dock_mode: false,
            profile_forced: false,
            notify: "bell".into(),
            #[cfg(test)]
            sent_notices: vec![],
            tty: false,
            mouse: true,
            drag: None,
            dragging: false,
            region: Default::default(),
            panes: vec![],
            snap: vec![],
            clicks: (Instant::now(), (0, 0), 0),
            anchor: None,
            input_rect: Default::default(),
            input_skip: 0,
            box_live: false,
            pending_copy: None,
            copied: String::new(),
            copied_at: None,
            sel_dirty: false,
            term_title: String::new(),
            dock: None,
            dock_focus: false,
            list_cache: None,
            list_loading: false,
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
            items_ui: None,
            open_ui: None,
            wt_ui: None,
            pin: None,
            shell_login: true,
            versions_dir: version::versions_dir(),
            line_items: segments::DEFAULT.to_vec(),
            instr: None,
            perms: None,
            opt: None,
            usage_open: false,
            btw_seq: 0,
            lanes: None,
            procs_ui: None,
            sup: None,
            sup_connecting: false,
            sup_tried: Instant::now() - Duration::from_secs(60),
            peers: vec![],
            ctx_ui: None,
            queue_ui: None,
            sched_ui: None,
            sched_at: None,
            search: None,
            vim: Default::default(),
            queue_on: true,
            sup_ui: None,
            sup_paths: supervisor::Paths::user(),
            limit_warn: 90,
            limit_hold: 0,
            limit_warned: vec![],
            limit_released: None,
            share_mcp: false,
            agent_term: false,
            term_ui: None,
            term_view: None,
            edit_request: false,
            editor_mode: false,
            pane: None,
            pane_seq: 0,
            pane_shape: 0,
            pane_old: None,
            pane_failed: false,
            stable_prompt: true,
            autocompact_k: 0,
            precompact_k: 0,
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
            focus: None,
            focus_at: Instant::now(),
            status_for: 0,
            startup_note: None,
            default_profile: "full".into(),
            cache_ttl: Duration::from_secs(3600),
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

    /// Some panel owns the keyboard (the input box is not the focus).
    fn overlay_open(&self) -> bool {
        self.front_panel().is_some() || self.term_view.is_some()
    }

    /// Directory of the tab in front: what `@`, the status line and history are relative to.
    fn cwd(&self) -> &str {
        self.sessions.get(self.active).and_then(|s| s.cwd.as_deref()).unwrap_or(&self.cwd)
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some(theme::keys(&msg.into()));
        self.flash_at = Instant::now();
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
                        if let Some(p) = cache_path("commands.json") {
                            write_commands(&p, &self.known_commands);
                        }
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
                self.focus = self.focus.map(|_| true); // a key reached this window: whatever the terminal said, you are here
                self.drag = None;
                self.s().sel.clear();
                self.on_key(k)
            }
            AppEvent::Term(Event::Paste(p)) => {
                self.last_input = Instant::now();
                self.focus = self.focus.map(|_| true);
                self.on_paste(p)
            }
            AppEvent::Term(Event::FocusGained) => {
                (self.focus, self.focus_at, self.last_input) = (Some(true), Instant::now(), Instant::now()); // you are back
            }
            AppEvent::Term(Event::FocusLost) => (self.focus, self.focus_at) = (Some(false), Instant::now()),
            AppEvent::Term(Event::Mouse(m)) => self.on_mouse(m),
            AppEvent::TermOut(_) => {} // the redraw after any event is the whole job
            AppEvent::Sessions(data) => {
                self.list_loading = false;
                let (here, prefer, open) = (self.cwd().to_string(), self.active_id(), self.open_states());
                for e in self.dock.iter_mut().chain(self.explorer.iter_mut()) {
                    e.reload(&here, &data, open.clone(), prefer.clone());
                }
                self.list_cache = Some(data);
            }
            AppEvent::Resumed(sid, read_no, path, loaded) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.finish_history(read_no, &path, *loaded);
                }
            }
            AppEvent::Btw(sid, id, res) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.side_done(id, res);
                }
            }
            AppEvent::Supervisor(c) => {
                self.sup_connecting = false;
                // A failed look for the shared supervisor leaves the window on the one it has.
                if c.is_some() || self.sup.is_none() {
                    // Shared MCP servers (`share_mcp`) run in the supervisor while there is one.
                    share::set_hub(c.as_ref().map(supervisor::Client::hub));
                    self.sup = c;
                }
            }
            AppEvent::Sup(supervisor::Msg::Peers(p)) => {
                self.peers = p;
                self.on_mem();
            }
            AppEvent::Sup(supervisor::Msg::Notice(text)) => self.flash(text),
            AppEvent::Sup(supervisor::Msg::Status(v)) => {
                if let Some(ui) = self.sup_ui.as_mut() {
                    ui.answered(v);
                }
            }
            AppEvent::Sup(supervisor::Msg::Suspend { tab, why, front }) => self.suspend_for_budget(tab, &why, front),
            AppEvent::Hint(n) => {
                if let Some(s) = self.sessions.first_mut() {
                    s.note(n);
                }
            }
            AppEvent::Procs(r) => {
                if let Some(ui) = self.procs_ui.as_mut() {
                    ui.set(r);
                }
            }
            AppEvent::Sent(mut loaded) => {
                loaded.append(&mut self.history); // anything sent since the start comes after what was read
                self.history = loaded;
            }
            AppEvent::Worktree(res) => match res {
                Ok((msg, open)) => {
                    self.flash(msg);
                    if let Some(p) = open {
                        self.open_folder_tab(&p);
                    }
                    self.wt_refresh();
                }
                Err(e) => {
                    self.flash(e);
                    self.wt_refresh();
                }
            },
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
        self.report_supervisor();
        self.warn_on_limits();
        self.refresh_supervisor();
        self.drain_queues(); // scheduled messages come due with the clock
        self.pump_statusline();
        for s in &mut self.sessions {
            s.poll_mcp();
        }
        self.opt_poll();
        if self.dock_at.elapsed() > Duration::from_secs(5) && self.dock.is_some() {
            self.load_sessions();
            self.dock_at = Instant::now();
        }
        if self.flash.is_some() && self.flash_at.elapsed() > Duration::from_secs(4) {
            self.flash = None;
        }
        self.precompact();
        let ttl = self.cache_ttl;
        for s in &mut self.sessions {
            if s.cache_left(ttl).is_some_and(|l| l.is_zero()) && !s.cold_noted {
                s.cold_noted = true;
                let k = s.ctx_tokens / 1000;
                s.note(format!("prompt cache expired after {}m idle: your next message re-caches ~{k}k tokens (a fresh /new tab would not)", ttl.as_secs() / 60));
            }
        }
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
}

#[cfg(test)]
mod tests;
