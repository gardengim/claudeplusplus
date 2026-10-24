mod backend;
mod config;
mod explorer;
mod jobs;
mod profile;
mod tabcfg;
mod usage;
mod files;
mod history;
mod image;
mod markdown;
mod panel;
mod prompt;
mod session;
mod settings;
mod statusline;
mod theme;
mod ui;

use backend::{Backend, Launch, Msg, Resume};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, MouseEventKind, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
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
use std::time::{Duration, Instant};

pub enum AppEvent {
    Term(Event),
    /// (session id, backend pid, message)
    Backend(u64, u32, Msg),
    /// A status line run finished: session id, run generation, and the output lines (None = failed/cancelled).
    Status(u64, u64, Option<Vec<String>>),
    /// `claude stop` finished: the message to show, and the session to open afterwards if it worked.
    Stopped(Result<String, String>, Option<(String, String, Option<String>)>),
    /// SIGTERM or SIGHUP: leave through the normal path so the terminal is restored.
    Quit,
}

/// `--profile lean` = this tool set, no MCP, and no user-level settings (user CLAUDE.md/AGENTS.md,
/// skills, hooks). Measured on Claude Code 2.1.309 in this repo: 65k → 7k prompt tokens.
use tabcfg::{MODES, Row};

const HELP: &str = "\
claude++ — a lighter native frontend for headless Claude Code

USAGE: claude++ [options] [-- <extra claude args>]

  --profile <name>            profile for new tabs: full|sources|lean or one of yours (see PROFILES) (change per tab with /profile, /new <profile>)
                        sources = project+local settings only; lean = that plus core tools, no MCP
  --sources <list>      which settings to load: user,project,local (default all). Excluding
                        `user` drops your ~/.claude CLAUDE.md/AGENTS.md, skills, hooks, plugins
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
  --bench [prompt]      measure prompt tokens / memory per profile, then exit

KEYS  enter send · alt/shift+enter or ctrl+j newline · esc/ctrl+c interrupt
      ctrl+t new tab · ctrl+n/ctrl+p next/prev tab · alt+1..9 jump · ctrl+d close tab
      shift+tab permission mode · ctrl+o expand tool output · wheel or pgup/pgdn scroll · ctrl+end jump to latest
      ctrl+r browse earlier sessions · tab completes a /command or an @file (↑↓ to pick, esc hides)
      ctrl+v paste an image as [Image #N] at the cursor (delete the token to drop it)
COMMANDS  /usage /find /session /new [profile] /profile [name] [--hard] /close /clear /resume [id] /model [m] /effort [level] /mode <m>
          /config /statusline [claude|builtin] /img <path> /quit
          anything else starting with / goes to Claude Code (e.g. /compact)
@FILES    type @ to pick a file or folder; Claude Code reads what you mention (images included)
PROFILES  your own: ~/.config/claude++/profiles/<name>.json (system prompt, tools, MCP, model, env...; see README)
EDITING   alt+←/→ word · alt+backspace / ctrl+w delete word · ctrl+k to end · ctrl+z undo · ctrl+y or /copy [code] copies the last answer
IMAGES    ctrl+v, drag a file into the window or /img <path> to attach; pictures in the transcript are drawn
          inline in kitty/Ghostty (CLAUDEPP_IMAGES=on|off overrides detection)";

/// Commands claude++ handles itself: (name, argument hint, description).
const LOCAL_COMMANDS: [(&str, &str, &str); 18] = [
    ("new", "[profile]", "open a new tab"),
    ("profile", "[name] [--hard]", "show or change this tab's profile"),
    ("close", "", "close this tab"),
    ("clear", "", "start a fresh conversation in this tab"),
    ("resume", "[id]", "browse and reopen earlier sessions"),
    ("sessions", "", "list sessions: this directory's and background ones (also ←)"),
    ("model", "[name]", "pick a model and effort, or switch directly"),
    ("effort", "[level]", "pick or set the thinking effort"),
    ("mode", "<mode>", "set the permission mode"),
    ("img", "<path>", "attach an image to the next message"),
    ("find", "<text>", "jump to the newest match in the conversation; again for older"),
    ("copy", "[code]", "copy the last answer, or its last code block, to the clipboard"),
    ("config", "", "claude++'s own config"),
    ("session", "", "this tab's permission mode and tool rules"),
    ("usage", "", "where this tab's context goes, and your plan limits"),
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
    mouse: bool,
    pub dock: Option<explorer::Explorer>,
    pub dock_focus: bool,
    dock_at: Instant,
    /// Slash commands Claude Code offers, remembered across runs so completion works before any
    /// backend is running.
    known_commands: Vec<Cmd>,
    /// Highlighted row of the `/command` and `@file` popup.
    pub sel: usize,
    /// Models Claude Code offers, remembered like the commands.
    known_models: Vec<ModelInfo>,
    pub model_picker: Option<ModelPicker>,
    pub settings: Option<settings::Ui>,
    pub tabcfg: Option<tabcfg::Ui>,
    pub usage_open: bool,
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
    bench: Option<String>,
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

/// Put the terminal back the way we found it: mouse reporting off, paste and key modes reset, images
/// deleted, raw mode and the alternate screen left. Safe to call twice (the panic hook and normal exit).
fn restore_terminal() {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[?1000l\x1b[?1006l");
    if ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste);
    image::clear_all();
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
        launch: Launch { bin: "claude".into(), ..Default::default() },
        resume: None,
        cont: false,
        idle: None,
        mem_budget_mb: None,
        profile: String::new(),
        cache_ttl_min: None,
        statusline: String::new(),
        bench: None,
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
            "--bench" => o.bench = Some("Reply with exactly: OK".into()),
            "--" => extra.extend(a.by_ref()),
            other if o.bench.is_some() && !other.starts_with('-') => o.bench = Some(other.into()),
            other => return Err(format!("unknown option {other}\n\n{HELP}")),
        }
    }
    profile_args(if profile.is_empty() { "full" } else { &profile })?; // validate; each tab applies its own profile at launch
    o.profile = profile;
    o.launch.args = extra;
    Ok(o)
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(msg) => {
            println!("{msg}");
            std::process::exit(if msg == HELP { 0 } else { 2 });
        }
    };
    if let Some(prompt) = opts.bench {
        std::process::exit(bench(&opts.launch, &prompt));
    }
    if let Err(e) = run(opts) {
        eprintln!("claude++: {e}");
        std::process::exit(1);
    }
}

fn run(opts: Opts) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    // Every setting: a flag wins for this run, else what /config saved, else the built-in default.
    config::migrate_dirs();
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
    app.dock_mode = saved("side_panel").as_deref() == Some("on");
    if let Some(n) = saved("notify").filter(|n| ["off", "bell", "desktop"].contains(&n.as_str())) {
        app.notify = n;
    }
    let mode = statusline::Mode::parse(&opts.statusline).or_else(|| saved("statusline").as_deref().and_then(statusline::Mode::parse));
    app.startup_note = app.apply_statusline_mode(mode.unwrap_or(statusline::Mode::Builtin));
    app.known_commands = load_commands();
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
        restore_terminal();
        previous(info);
    }));
    thread::spawn(move || {
        while let Ok(e) = event::read() {
            if tx.send(AppEvent::Term(e)).is_err() {
                break;
            }
        }
    });

    let res = (|| -> io::Result<()> {
        while !app.quit {
            term.draw(|f| ui::draw(f, &mut app))?;
            let wait = if app.sessions.iter().any(|s| s.busy) { 100 } else { 1000 };
            match rx.recv_timeout(Duration::from_millis(wait)) {
                Ok(ev) => app.on_event(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Coalesce bursts (streaming deltas) into a single redraw.
            while let Ok(ev) = rx.try_recv() {
                app.on_event(ev);
            }
            app.tick();
        }
        Ok(())
    })();

    restore_terminal();
    // EOF every backend first so they shut down in parallel, then wait for each.
    app.sessions.iter_mut().filter_map(|s| s.backend.as_mut()).for_each(Backend::close);
    drop(app);
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
            dock_mode: false,
            profile_forced: false,
            notify: "bell".into(),
            #[cfg(test)]
            sent_notices: vec![],
            tty: false,
            mouse: true,
            dock: None,
            dock_focus: false,
            dock_at: Instant::now(),
            known_commands: vec![],
            sel: 0,
            known_models: vec![],
            model_picker: None,
            settings: None,
            tabcfg: None,
            usage_open: false,
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
        if wanted {
            let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
            self.s().warm(&launch, &tx);
        }
    }

    fn s(&mut self) -> &mut Session {
        &mut self.sessions[self.active]
    }

    pub fn placeholder(&self) -> &'static str {
        if !self.sessions[self.active].pending.is_empty() {
            "answer in the panel above…"
        } else if self.explorer.is_some() || self.model_picker.is_some() || self.settings.is_some() || self.tabcfg.is_some() || self.usage_open {
            "use the panel above…"
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
        l.args = args;
        l.cwd = self.sessions[i].cwd.clone();
        l
    }

    /// Directory of the tab in front: what `@`, the status line and history are relative to.
    fn cwd(&self) -> &str {
        self.sessions[self.active].cwd.as_deref().unwrap_or(&self.cwd)
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some(msg.into());
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
        self.save_draft();
        self.sessions.push(s);
        self.active = self.sessions.len() - 1;
        self.load_draft();
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
            self.save_draft();
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
            }
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
                self.on_key(k)
            }
            AppEvent::Term(Event::Paste(p)) => {
                self.last_input = Instant::now();
                self.on_paste(p)
            }
            AppEvent::Term(Event::Mouse(m)) => {
                let s = self.s();
                match m.kind {
                    MouseEventKind::ScrollUp => s.scroll += 3,
                    MouseEventKind::ScrollDown => s.scroll = s.scroll.saturating_sub(3),
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
        self.pump_statusline();
        if self.dock_at.elapsed() > Duration::from_secs(5) && self.dock.is_some() {
            let (here, open) = (self.cwd().to_string(), self.sessions.iter().filter_map(|s| s.session_id.clone()).collect());
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
                if i != self.active && s.last_active.elapsed() > self.idle_suspend && s.suspend() {
                    s.note("idle: claude process stopped to free memory; it resumes on your next message".into());
                }
            }
        }
        let roots: Vec<u32> = self.sessions.iter().filter_map(|s| s.backend.as_ref().map(|b| b.pid)).collect();
        let mem = tree_mem_kb(&roots);
        for s in &mut self.sessions {
            s.mem_kb = s.backend.as_ref().and_then(|b| mem.iter().find(|r| r.0 == b.pid)).map_or(0, |r| r.1);
        }
        // Our own process only: its tree would include the backends, which are counted above.
        let me = std::process::id();
        let own = footprint_kb(&[me]);
        self.ui_mem_kb = own.first().or(rss_kb(&[me]).first()).map_or(0, |r| r.1);
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
        if self.explorer.is_some() || self.model_picker.is_some() || self.settings.is_some() || self.tabcfg.is_some() || self.usage_open {
            return;
        }
        // A question takes pasted text as its typed answer; a permission prompt takes nothing (a pasted "y"
        // must never approve anything).
        if let Some(pending) = self.sessions[self.active].pending.front_mut() {
            if pending.is_question() {
                let qs = pending.questions().to_vec();
                pending.ui.question_paste(&qs, &p);
            }
            return;
        }
        if p.is_empty() {
            // Terminals send an empty paste when the clipboard holds an image and no text.
            if !self.paste_image() {
                self.flash("nothing to paste");
            }
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

    fn paste_image(&mut self) -> bool {
        let Some(png) = image::clipboard_png() else { return false };
        self.clip_n += 1;
        self.attach_bytes(format!("clipboard-{}.png", self.clip_n), png, "image/png");
        true
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

    fn on_key(&mut self, k: KeyEvent) {
        let box_is_free = self.explorer.is_none() && self.model_picker.is_none() && self.settings.is_none() && self.tabcfg.is_none() && !self.usage_open && self.sessions[self.active].pending.is_empty();
        if box_is_free && k.code == KeyCode::Char('z') && k.modifiers.contains(KeyModifiers::CONTROL) {
            return match self.undo.pop() {
                Some((text, cursor)) => (self.input, self.cursor) = (text, cursor),
                None => self.flash("nothing to undo"),
            };
        }
        let before = (self.input.clone(), self.cursor);
        self.reset_undo = false;
        self.handle_key(k);
        if std::mem::take(&mut self.reset_undo) {
            self.undo.clear(); // sent, or a different tab's box: nothing earlier belongs to this one
        } else if self.input != before.0 {
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
            self.drop_orphan_images();
        }
        self.refresh_files();
    }

    /// Attachments whose `[Image #n]` token was deleted from the text are not sent; free them now.
    fn drop_orphan_images(&mut self) {
        if self.sessions[self.active].attachments.is_empty() {
            return;
        }
        let present = image::tokens_in(&self.input);
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
        let tok = if self.explorer.is_some() || self.model_picker.is_some() || self.settings.is_some() || self.tabcfg.is_some() || self.usage_open { None } else { self.at_token() };
        let Some((_, q)) = tok else {
            self.file_rows.clear();
            self.file_key = None;
            return;
        };
        if self.file_key.as_deref() == Some(q.as_str()) {
            return;
        }
        if self.file_key.is_none() && self.files_at.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            self.files = files::list(self.cwd());
            self.files_at = Some(Instant::now());
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
            .map(|(n, h, d)| (true, Completion { prefix: '/', name: n.to_string(), hint: h.to_string(), desc: d.to_string() }))
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

    fn set_mouse(&mut self, on: bool) {
        self.mouse = on;
        if self.tty {
            let _ = write!(io::stdout(), "\x1b[?1000{0}\x1b[?1006{0}", if on { 'h' } else { 'l' });
            let _ = io::stdout().flush();
        }
    }

    #[cfg(test)]
    fn profile_flag_error(&self, p: &str) -> bool {
        profile_args(p).is_err()
    }

    fn show_dock(&mut self, focus: bool) {
        if self.dock.is_none() {
            let open = self.sessions.iter().filter_map(|s| s.session_id.clone()).collect();
            self.dock = Some(explorer::Explorer::new(self.cwd(), open));
            self.dock_at = Instant::now();
        }
        self.dock_focus = focus;
    }

    /// ctrl+r and /sessions: the docked list when configured (toggle), else the full-screen one.
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
            KeyCode::Right => (self.dock, self.dock_focus) = (None, false),
            KeyCode::Esc => self.dock_focus = false,
            _ => {
                if let Some(o) = self.dock.as_mut().map(|d| d.key(k)) {
                    self.explorer_outcome(o); // the dock keeps the keyboard; typing hands it back
                }
            }
        }
    }

    /// What a list row asked for (open, fork, stop...), from the full-screen list or the dock.
    fn explorer_outcome(&mut self, o: explorer::Outcome) {
        use explorer::Outcome::*;
        match o {
            Open(id, title, cwd) => self.open_session(id, title, cwd),
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
        let open = self.sessions.iter().filter_map(|s| s.session_id.clone()).collect();
        self.explorer = Some(explorer::Explorer::new(self.cwd(), open));
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
        if self.model_picker.is_some() {
            return self.picker_key(k);
        }
        if self.dock_focus && self.dock.is_some() && self.settings.is_none() && self.tabcfg.is_none() && !self.usage_open && self.sessions[self.active].pending.is_empty() {
            use KeyCode::*;
            if matches!(k.code, Up | Down | PageUp | PageDown | Home | End | Enter | Right | Esc | Left) && !ctrl && !alt {
                return self.dock_key(k);
            }
            self.dock_focus = false; // any other key is for the input box
        }
        if self.settings.is_some() {
            return self.settings_key(k);
        }
        if self.tabcfg.is_some() {
            return self.tabcfg_key(k);
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
                KeyCode::Up => return self.sel = (sel + rows.len() - 1) % rows.len(),
                KeyCode::Down => return self.sel = (sel + 1) % rows.len(),
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

        // A permission or question panel owns the keyboard, except keys the app keeps (ctrl/alt chords for
        // tabs, interrupt..., and page keys that scroll the transcript behind it).
        let scrolling = matches!(k.code, KeyCode::PageUp | KeyCode::PageDown);
        if !ctrl && !alt && !scrolling && !self.sessions[self.active].pending.is_empty() {
            return self.prompt_key(k);
        }

        match k.code {
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
            KeyCode::Esc => self.s().interrupt(),
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.close_tab_asking(),
            KeyCode::Char('t') if ctrl => self.new_tab(),
            KeyCode::Char('r') if ctrl => self.toggle_sessions(),
            KeyCode::Char('v') if ctrl => {
                if !self.paste_image() {
                    self.flash("no image on the clipboard (ctrl+v pastes images; paste text with your terminal's paste)");
                }
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
                    s.items.iter_mut().for_each(|i| i.cache = None);
                }
            }
            KeyCode::Char('y') if ctrl => self.copy_last(""),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => self.insert("\n"),
            KeyCode::Enter => self.submit(),
            KeyCode::BackTab => {
                let next = MODES[(MODES.iter().position(|m| *m == self.s().mode).map_or(0, |i| i + 1)) % MODES.len()];
                self.set_mode(next);
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.input.chars().count(),
            KeyCode::Char('u') if ctrl => self.set_input(String::new()),
            KeyCode::Char('w') if ctrl => self.delete_range(self.word_left(), self.cursor),
            KeyCode::Backspace if alt => self.delete_range(self.word_left(), self.cursor),
            KeyCode::Char('d') if alt => self.delete_range(self.cursor, self.word_right()),
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
            KeyCode::Char('k') if ctrl => self.delete_range(self.cursor, self.input.chars().count()),
            KeyCode::Char(c) if !ctrl => {
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
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => self.s().scroll += 1,
            KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => {
                self.s().scroll = self.s().scroll.saturating_sub(1)
            }
            KeyCode::PageUp => self.s().scroll += self.s().view_h.saturating_sub(2).max(1),
            KeyCode::PageDown => self.s().scroll = self.s().scroll.saturating_sub(self.s().view_h.saturating_sub(2).max(1)),
            KeyCode::Up => self.history_step(-1),
            KeyCode::Down => self.history_step(1),
            _ => {}
        }
    }

    fn history_step(&mut self, d: isize) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        let pos = match (self.hist_pos, d) {
            (None, -1) => Some(n - 1),
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(p), -1) => Some(p - 1),
            (Some(p), _) if p + 1 < n => Some(p + 1),
            (Some(_), _) => None,
        };
        self.hist_pos = pos;
        self.set_input(pos.map(|p| self.history[p].clone()).unwrap_or_default());
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
        let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
        let s = self.s();
        s.usage = Default::default();
        s.warm(&launch, &tx);
        s.control("get_context_usage", json!({}));
        s.control("get_usage", json!({}));
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
            (KeyCode::Up, _) => ui.sel = (sel + rows.len() - 1) % rows.len(),
            (KeyCode::Down, _) => ui.sel = (sel + 1) % rows.len(),
            (KeyCode::Enter, Row::Add) | (_, Row::Add) if delta != 0 || k.code == KeyCode::Enter => ui.adding = Some(String::new()),
            (_, Row::Mode) if delta != 0 || k.code == KeyCode::Enter => {
                let cur = MODES.iter().position(|m| *m == self.sessions[self.active].mode).map_or(0, |i| i as isize);
                let next = MODES[(cur + if delta == 0 { 1 } else { delta }).rem_euclid(MODES.len() as isize) as usize];
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
        let Some(mut ui) = self.settings else { return };
        match k.code {
            KeyCode::Esc => return self.settings = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return self.settings = None,
            KeyCode::Up => ui.step(-1),
            KeyCode::Down => ui.step(1),
            KeyCode::Left | KeyCode::BackTab => self.change_setting(ui.sel, -1),
            KeyCode::Right | KeyCode::Tab | KeyCode::Enter | KeyCode::Char(' ') => self.change_setting(ui.sel, 1),
            _ => {}
        }
        self.settings = Some(ui);
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
            "side_panel" => if self.dock_mode { "on" } else { "off" }.to_string(),
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
            "notify" => self.notify = value.into(),
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

    fn picker_key(&mut self, k: KeyEvent) {
        let n = self.model_list().len();
        let Some(p) = self.model_picker else { return };
        let levels = |sel: usize| self.model_list().get(sel).map_or(0, |m| m.efforts.len());
        let next = match k.code {
            KeyCode::Esc => return self.model_picker = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return self.model_picker = None,
            KeyCode::Enter => return self.apply_model_picker(),
            KeyCode::Up | KeyCode::Down if n > 0 => {
                let new = if k.code == KeyCode::Up { (p.sel + n - 1) % n } else { (p.sel + 1) % n };
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
        self.reset_undo = true;
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_pos = None;
        if text.trim().is_empty() && self.s().attachments.is_empty() {
            return;
        }
        // Recalled text cannot bring a pasted picture back, so history keeps the words only.
        let words = image::strip_tokens(&text);
        if self.history.last() != Some(&words) && !words.trim().is_empty() {
            self.history.push(words);
        }
        let (cmd, arg) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
        let arg = arg.trim().to_string();
        match cmd {
            "/quit" | "/exit" => self.quit = true,
            "/help" => self.s().note(HELP.into()),
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
                let found = history::recent(self.cwd(), 500).into_iter().find(|f| f.id.starts_with(&arg));
                match (found, history::find(&arg)) {
                    (Some(f), _) => self.open_session(f.id, f.title, None),
                    (None, Some(_)) => self.open_session(arg, String::new(), None),
                    (None, None) => self.s().note(format!("no session starting with {arg:?} here; /resume lists them")),
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
            "/config" => self.settings = Some(settings::Ui::default()),
            "/session" => self.tabcfg = Some(tabcfg::Ui::default()),
            "/usage" => self.open_usage(),
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
                let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
                let mentioned = self.mentioned_images(&text);
                self.s().submit(text, &mentioned, &launch, &tx);
            }
        }
    }
}

fn cache_path(name: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))?;
    Some(base.join(config::DIR).join(name))
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

/// One-shot measurement of each profile: same prompt, fresh unsaved session.
fn bench(base: &Launch, prompt: &str) -> i32 {
    let passthrough = base.args.clone();
    println!("prompt: {prompt:?}\n");
    println!("{:<6} {:>6} {:>9} {:>14} {:>8} {:>8} {:>9}", "prof", "tools", "init ms", "prompt tokens", "output", "cost $", "peak RSS");
    for profile in ["full", "lean"] {
        let mut args = profile_args(profile).unwrap();
        args.extend(passthrough.iter().cloned());
        args.push("--no-session-persistence".into());
        let launch = Launch { bin: base.bin.clone(), args, env: base.env.clone(), cwd: None };
        let (tx, rx) = mpsc::channel();
        let t0 = Instant::now();
        let mut b = match Backend::spawn(&launch, Resume::New, tx, |_, m| m) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("spawn failed: {e}");
                return 1;
            }
        };
        if b.user(json!(prompt)).is_err() {
            return 1;
        }
        let (mut init_ms, mut tools, mut peak) = (0, 0, 0);
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Msg::Line(v)) if v["type"] == "system" && v["subtype"] == "init" && init_ms == 0 => {
                    init_ms = t0.elapsed().as_millis();
                    tools = v["tools"].as_array().map_or(0, Vec::len);
                }
                Ok(Msg::Line(v)) if v["type"] == "result" => {
                    let u = &v["usage"];
                    let n = |k: &str| u[k].as_u64().unwrap_or(0);
                    let prompt_tokens = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
                    peak = peak.max(rss_kb(&[b.pid]).first().map_or(0, |r| r.1));
                    println!(
                        "{profile:<6} {tools:>6} {init_ms:>9} {prompt_tokens:>14} {:>8} {:>8.4} {:>8}M",
                        n("output_tokens"),
                        v["total_cost_usd"].as_f64().unwrap_or(0.0),
                        peak / 1024
                    );
                    break;
                }
                Ok(Msg::Exited { stderr_tail, .. }) => {
                    eprintln!("{profile}: claude exited early\n{stderr_tail}");
                    return 1;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => peak = peak.max(rss_kb(&[b.pid]).first().map_or(0, |r| r.1)),
                Err(RecvTimeoutError::Disconnected) => return 1,
            }
        }
    }
    println!("\nprompt tokens = input + cache read + cache write for the turn (cache reads bill at ~10%).");
    0
}

#[cfg(test)]
mod tests;
