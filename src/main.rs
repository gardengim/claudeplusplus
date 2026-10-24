mod backend;
mod config;
mod explorer;
mod jobs;
mod profile;
mod tabcfg;
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
use session::{Choice, Cmd, ModelInfo, Session};
use std::path::PathBuf;
use std::io::{self, Write};
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
  --idle-suspend <s>    stop idle background tabs' claude after s seconds (0 = never, default 600)
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
COMMANDS  /session /new [profile] /profile [name] [--hard] /close /clear /resume [id] /model [m] /effort [level] /mode <m>
          /config /statusline [claude|builtin] /img <path> /quit
          anything else starting with / goes to Claude Code (e.g. /compact)
@FILES    type @ to pick a file or folder; Claude Code reads what you mention (images included)
PROFILES  your own: ~/.config/claude++/profiles/<name>.json (system prompt, tools, MCP, model, env...; see README)
IMAGES    ctrl+v, drag a file into the window or /img <path> to attach; pictures in the transcript are drawn
          inline in kitty/Ghostty (CLAUDEPP_IMAGES=on|off overrides detection)";

/// Commands claude++ handles itself: (name, argument hint, description).
const LOCAL_COMMANDS: [(&str, &str, &str); 15] = [
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
    ("config", "", "claude++'s own config"),
    ("session", "", "this tab's permission mode and tool rules"),
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
    let mut profile = "full".to_string();
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
    profile_args(&profile)?; // validate; each tab applies its own profile at launch
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
    let idle = opts.idle.or(saved_num("idle_suspend_s")).unwrap_or(600);
    let mut app = App::new(opts.launch, cwd, tx.clone(), Duration::from_secs(idle));
    app.config_path = config_path.clone();
    app.mem_budget_kb = opts.mem_budget_mb.or(saved_num("mem_budget_mb")).unwrap_or(0) * 1024;
    let profile = Some(opts.profile).filter(|p| !p.is_empty()).or_else(|| saved("profile").filter(|p| profile_args(p).is_ok()));
    if let Some(p) = profile {
        app.default_profile = p;
    }
    app.cache_ttl = Duration::from_secs(opts.cache_ttl_min.or(saved_num("cache_ttl_min")).unwrap_or(60) * 60);
    image::set_mode(&saved("images").unwrap_or_default());
    app.dock_mode = saved("side_panel").as_deref() == Some("on");
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
    }
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

    if enhanced {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    app.set_mouse(false);
    image::clear_all();
    ratatui::restore();
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
        } else if self.explorer.is_some() || self.model_picker.is_some() || self.settings.is_some() || self.tabcfg.is_some() {
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
        s.cache_ttl = self.cache_ttl;
        self.sessions.push(s);
        self.active = self.sessions.len() - 1;
    }

    fn close_tab(&mut self) {
        self.sessions.remove(self.active);
        if self.sessions.is_empty() {
            self.quit = true;
            self.new_tab(); // keep `active` valid until the loop exits
        }
        self.active = self.active.min(self.sessions.len() - 1);
    }

    fn switch(&mut self, delta: isize) {
        let n = self.sessions.len() as isize;
        self.active = ((self.active as isize + delta).rem_euclid(n)) as usize;
        self.files_at = None; // the `@` index belongs to the tab's directory
    }

    fn on_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Backend(sid, pid, msg) => {
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
        if self.last_sample.elapsed() < Duration::from_secs(5) {
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
        if self.explorer.is_some() || self.model_picker.is_some() || self.settings.is_some() || self.tabcfg.is_some() {
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

    fn byte_at(&self, char_idx: usize) -> usize {
        self.input.char_indices().nth(char_idx).map_or(self.input.len(), |(b, _)| b)
    }

    fn set_input(&mut self, t: String) {
        self.cursor = t.chars().count();
        self.input = t;
    }

    fn on_key(&mut self, k: KeyEvent) {
        let before = self.input.clone();
        self.handle_key(k);
        if self.input != before {
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
        let tok = if self.explorer.is_some() || self.model_picker.is_some() || self.settings.is_some() || self.tabcfg.is_some() { None } else { self.at_token() };
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
            self.active = i;
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
        if self.dock_focus && self.dock.is_some() && self.settings.is_none() && self.tabcfg.is_none() && self.sessions[self.active].pending.is_empty() {
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
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.close_tab(),
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
                    self.active = i;
                }
            }
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                for s in &mut self.sessions {
                    s.items.iter_mut().for_each(|i| i.cache = None);
                }
            }
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
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.input.chars().collect();
                let mut i = self.cursor;
                while i > 0 && chars[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && chars[i - 1] != ' ' {
                    i -= 1;
                }
                let (a, b) = (self.byte_at(i), self.byte_at(self.cursor));
                self.input.replace_range(a..b, "");
                self.cursor = i;
            }
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
            "/close" => self.close_tab(),
            "/clear" => self.s().clear(),
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
mod tests {
    use super::*;

    fn run_cmd(app: &mut App, cmd: &str) {
        app.input = cmd.into();
        app.cursor = cmd.chars().count();
        app.submit();
    }

    #[test]
    fn profiles_apply_per_tab_and_model_switch_warns_about_recache() {
        let (mut app, dir) = fake_app("prof");
        app.on_key(key('h')); // tab 0, default profile: spawns with no profile flags
        run_cmd(&mut app, "/new lean");
        assert_eq!(app.sessions[1].profile, "lean");
        app.on_key(key('x')); // tab 1: lean flags
        wait_spawns(&dir, 2);
        let log = std::fs::read_to_string(dir.join("spawns")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2);
        // The two scripts append concurrently, so don't depend on line order.
        let lean: Vec<_> = lines.iter().filter(|l| l.contains("--tools")).collect();
        assert_eq!(lean.len(), 1, "{lines:?}");
        assert!(lean[0].contains("--setting-sources project,local"), "{}", lean[0]);

        // Mid-conversation switch: backend is dropped (next message resumes with new flags) and the note
        // says what it costs; refused while a turn is running.
        app.sessions[1].ctx_tokens = 12_000;
        run_cmd(&mut app, "/profile full");
        assert!(app.sessions[1].backend.is_none() && app.sessions[1].profile == "full");
        let last = app.sessions[1].items.last().unwrap();
        assert!(matches!(&last.entry, session::Entry::Note(t) if t.contains("keeps the system prompt")));
        assert!(!app.launch_for(1).args.contains(&"--system-prompt-snapshot".to_string()));
        run_cmd(&mut app, "/profile lean --hard");
        assert!(app.sessions[1].unpinned && app.sessions[1].profile == "lean");
        let a = app.launch_for(1).args;
        assert!(a.windows(2).any(|w| w == ["--system-prompt-snapshot", "off"]), "{a:?}");
        let last = app.sessions[1].items.last().unwrap();
        assert!(matches!(&last.entry, session::Entry::Note(t) if t.contains("re-caches up to ~12k")));
        run_cmd(&mut app, "/model sonnet");
        let last = app.sessions[1].items.last().unwrap();
        assert!(matches!(&last.entry, session::Entry::Note(t) if t.contains("re-caches ~12k")), "model switch warns");
        app.sessions[1].busy = true;
        run_cmd(&mut app, "/profile sources");
        assert_eq!(app.sessions[1].profile, "lean");
        assert!(app.flash.is_some());
        run_cmd(&mut app, "/new bogus");
        assert_eq!(app.sessions.len(), 2, "unknown profile opens nothing");
    }

    #[test]
    fn cache_clock_counts_down_from_the_last_api_response() {
        let mut s = Session::new(1, String::new());
        let ttl = Duration::from_secs(3600);
        assert!(s.cache_left(ttl).is_none(), "no conversation, no cache");
        s.ctx_tokens = 5000;
        s.last_api = Instant::now().checked_sub(Duration::from_secs(600));
        let left = s.cache_left(ttl).unwrap();
        assert!(left > Duration::from_secs(2900) && left < Duration::from_secs(3000));
        s.last_api = Instant::now().checked_sub(Duration::from_secs(4000));
        assert!(s.cache_left(ttl).unwrap().is_zero());
    }

    #[test]
    fn footprint_output_is_parsed() {
        let out = "======\nclaude [123]: 64-bit    Footprint: 1.5 GB (16384 bytes per page)\n======\nnode [45]: 64-bit    Footprint: 85 MB (16384 bytes per page)\nsleep [6]: 64-bit    Footprint: 1056 KB (16384 bytes per page)\n  Dirty   Clean\n";
        assert_eq!(parse_footprint(out), vec![(123, 1572864), (45, 87040), (6, 1056)]);
    }

    #[test]
    fn over_budget_stops_lru_idle_tab_but_never_the_focused_or_busy_one() {
        let (mut app, dir) = fake_app("budget");
        app.new_tab();
        app.new_tab(); // tabs 0,1,2; focus on 2
        for s in &mut app.sessions {
            s.session_id = Some("sid".into());
            s.backend = Some(Backend::spawn(&app.launch, Resume::New, app.tx.clone(), |_, m| AppEvent::Backend(0, 0, m)).unwrap());
            s.mem_kb = 100 * 1024;
        }
        app.sessions[0].last_active = Instant::now() - Duration::from_secs(100);
        app.sessions[1].last_active = Instant::now() - Duration::from_secs(50);
        app.mem_budget_kb = 250 * 1024;
        app.sessions[0].busy = true; // oldest is mid-turn: protected, so the next-oldest goes
        app.enforce_budget();
        assert!(app.sessions[0].backend.is_some() && app.sessions[1].backend.is_none() && app.sessions[2].backend.is_some());
        let _ = dir;
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn fake_app(tag: &str) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-warm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> {}/spawns\nwhile read l; do :; done\n", dir.display())).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let launch = Launch { bin: script.to_string_lossy().into(), ..Default::default() };
        let (tx, _rx) = mpsc::channel();
        std::mem::forget(_rx); // keep the channel open for reader threads
        let mut app = App::new(launch, "/".into(), tx, Duration::ZERO);
        app.new_tab();
        (app, dir)
    }

    fn count(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("spawns")).map_or(0, |s| s.lines().count())
    }

    /// Wait (up to 3 s) for `n` spawns to be logged, so a loaded machine cannot fail the test.
    fn wait_spawns(dir: &std::path::Path, n: usize) -> usize {
        let end = Instant::now() + Duration::from_secs(3);
        while count(dir) < n && Instant::now() < end {
            thread::sleep(Duration::from_millis(20));
        }
        count(dir)
    }

    /// For asserting that nothing spawned: there is nothing to wait for, so allow a grace period.
    fn spawns(dir: &std::path::Path) -> usize {
        thread::sleep(Duration::from_millis(300));
        count(dir)
    }

    #[test]
    fn backend_warms_on_first_keystroke_once_and_never_for_slash_commands() {
        let (mut app, dir) = fake_app("a");
        assert_eq!(spawns(&dir), 0, "idle UI spawns nothing");
        for c in "hello".chars() {
            app.on_key(key(c));
        }
        assert_eq!(wait_spawns(&dir, 1), 1, "typing spawns one backend, not one per key");
        assert!(app.sessions[0].items.is_empty(), "warming sends no message");

        let (mut app2, dir2) = fake_app("b");
        app2.known_commands = vec![cmd("compact", "", "summarize")];
        for c in "/new".chars() {
            app2.on_key(key(c));
        }
        assert_eq!(spawns(&dir2), 0, "slash commands do not spawn once their list is known");

        let (mut app3, dir3) = fake_app("c");
        app3.on_key(key('/'));
        assert_eq!(wait_spawns(&dir3, 1), 1, "the very first `/` starts a backend to learn the command list");
    }

    fn cmd(name: &str, hint: &str, desc: &str) -> Cmd {
        Cmd { name: name.into(), hint: hint.into(), desc: desc.into() }
    }
    fn typed(app: &mut App, text: &str) {
        text.chars().for_each(|c| app.on_key(key(c)));
    }
    fn names(app: &App) -> Vec<String> {
        app.completions().into_iter().map(|c| c.name).collect()
    }
    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn completion_ranks_prefix_first_and_dedupes_local_commands() {
        let (mut app, _d) = fake_app("comp");
        app.known_commands = vec![cmd("compact", "[focus]", "summarize"), cmd("model", "", "theirs"), cmd("context", "", "c")];
        typed(&mut app, "/c");
        assert_eq!(names(&app), ["clear", "close", "config", "compact", "context"]);
        press(&mut app, KeyCode::Backspace);
        typed(&mut app, "m");
        // prefix matches ("mode", "model") before names that merely contain "m"; ours before theirs
        assert_eq!(names(&app), ["mode", "model", "img", "resume", "compact"]);
        let model = app.completions().into_iter().find(|c| c.name == "model").unwrap();
        assert_eq!(model.desc, "pick a model and effort, or switch directly", "ours wins over Claude Code's same-named command");
        assert!(names(&app).iter().filter(|n| *n == "model").count() == 1);
    }

    #[test]
    fn tab_and_enter_complete_a_partial_name_but_enter_runs_a_complete_one() {
        let (mut app, dir) = fake_app("tab");
        app.known_commands = vec![cmd("compact", "[focus]", "summarize")];
        typed(&mut app, "/comp");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "/compact ");
        assert!(app.completions().is_empty(), "popup closes once an argument may follow");

        app.set_input("/co".into());
        press(&mut app, KeyCode::Down);
        let picked = app.completions()[app.sel].name.clone();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.input, format!("/{picked} "), "Enter on a partial name completes instead of sending");
        assert!(app.sessions[0].items.is_empty());
        assert_eq!(spawns(&dir), 0);

        // A complete, exact name runs. /close on the only tab ends the app.
        app.set_input("/close".into());
        press(&mut app, KeyCode::Enter);
        assert!(app.quit);
    }

    #[test]
    fn no_popup_for_plain_text_arguments_or_a_pending_prompt() {
        let (mut app, _d) = fake_app("nopop");
        typed(&mut app, "hello /c");
        assert!(app.completions().is_empty());
        app.set_input("/model so".into());
        assert!(app.completions().is_empty());
    }

    #[test]
    fn clear_and_img_are_handled_locally() {
        let (mut app, dir) = fake_app("local");
        app.sessions[0].push(session::Entry::User("old".into()));
        run_cmd(&mut app, "/clear");
        assert!(app.sessions[0].items.is_empty());
        run_cmd(&mut app, "/img");
        assert!(app.flash.as_deref().is_some_and(|f| f.contains("usage")));
        run_cmd(&mut app, "/img /definitely/not/here.png");
        assert!(app.flash.as_deref().is_some_and(|f| f.contains("here.png")), "{:?}", app.flash);
        assert_eq!(spawns(&dir), 0, "none of these reach Claude Code");
        assert!(app.sessions[0].items.iter().all(|i| !matches!(i.entry, session::Entry::User(_))));
    }

    #[test]
    fn command_list_is_cached_on_disk() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-cmds-{}", std::process::id()));
        let path = dir.join("sub").join("commands.json");
        assert!(read_commands(&path).is_empty());
        write_commands(&path, &[cmd("compact", "[focus]", "summarize"), cmd("x", "", "")]);
        let back = read_commands(&path);
        assert_eq!((back.len(), back[0].name.as_str(), back[0].hint.as_str(), back[0].desc.as_str()), (2, "compact", "[focus]", "summarize"));
    }

    fn project(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-proj-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for f in ["src/main.rs", "src/session.rs", "docs/pic.png", "README.md"] {
            std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
            std::fs::write(dir.join(f), "x").unwrap();
        }
        dir
    }

    #[test]
    fn at_picker_completes_files_and_folders_and_hides_on_esc() {
        let (mut app, _d) = fake_app("at");
        app.cwd = project("at").to_string_lossy().into();
        typed(&mut app, "look at @sma");
        assert_eq!(names(&app)[0], "src/main.rs");
        assert!(app.completions().iter().all(|c| c.prefix == '@'));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "look at @src/main.rs ");
        assert!(app.completions().is_empty(), "a finished word closes the popup");

        app.set_input("@".into());
        app.refresh_files();
        assert_eq!(names(&app), ["docs/", "src/", "README.md"], "bare @ lists the top level");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // src/ : a folder keeps the popup open
        assert_eq!(app.input, "@src/");
        assert!(!app.completions().is_empty());
        assert!(app.sessions[0].items.is_empty(), "Enter picked a file, it did not send");
        typed(&mut app, "ses");
        assert_eq!(names(&app)[0], "src/session.rs");

        press(&mut app, KeyCode::Esc);
        assert!(app.completions().is_empty(), "esc hides the popup");
        typed(&mut app, "s");
        assert!(!app.completions().is_empty(), "typing brings it back");
        app.set_input("mail me@src".into());
        app.refresh_files();
        assert!(app.completions().is_empty(), "an @ inside a word is not a mention");
    }

    #[test]
    fn image_mentions_are_found_relative_to_the_project() {
        let (mut app, _d) = fake_app("mention");
        app.cwd = project("mention").to_string_lossy().into();
        let found = app.mentioned_images("see @docs/pic.png, and @src/main.rs and @missing.png and @docs/pic.png.");
        assert_eq!(found, vec![std::path::PathBuf::from(&app.cwd).join("docs/pic.png")]);
    }

    fn paste(app: &mut App) {
        app.attach_bytes("clip.png".into(), vec![1, 2, 3], "image/png");
    }
    fn visible_input(app: &App) -> String {
        app.input.replace('\n', "|")
    }

    #[test]
    fn a_pasted_image_becomes_a_token_on_its_own_line_at_the_cursor() {
        let (mut app, _d) = fake_app("tok");
        typed(&mut app, "this is text sample");
        paste(&mut app);
        assert_eq!(visible_input(&app), "this is text sample|[Image #1]|");
        typed(&mut app, "And this is pasted image.");
        assert_eq!(visible_input(&app), "this is text sample|[Image #1]|And this is pasted image.");

        // pasting in the middle of a line splits it; a second paste gets the next number
        app.set_input("before after".into());
        app.cursor = 7;
        paste(&mut app);
        assert_eq!(visible_input(&app), "before |[Image #2]|after");
        assert_eq!(app.sessions[0].attachments.iter().map(|a| a.n).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(app.input.chars().nth(app.cursor), Some('a'), "cursor lands after the token");
    }

    #[test]
    fn deleting_a_token_removes_it_whole_and_frees_the_attachment() {
        let (mut app, _d) = fake_app("deltok");
        typed(&mut app, "x");
        paste(&mut app);
        paste(&mut app);
        assert_eq!(visible_input(&app), "x|[Image #1]|[Image #2]|", "no blank line is added after a newline");
        // cursor is after "[Image #2]\n": step back over the newline, then one backspace takes the token
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(visible_input(&app), "x|[Image #1]|");
        assert_eq!(app.sessions[0].attachments.iter().map(|a| a.n).collect::<Vec<_>>(), [1], "image 2 is dropped with its token");
        // Delete in front of a token removes it whole too
        app.cursor = 2;
        press(&mut app, KeyCode::Delete);
        assert_eq!(visible_input(&app), "x||");
        assert!(app.sessions[0].attachments.is_empty());
        // a half-typed token is just text
        app.set_input("[Image #1".into());
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.input, "[Image #");
    }

    #[test]
    fn the_message_is_sent_with_text_and_pictures_in_the_order_written() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let launch = Launch { bin: script.to_string_lossy().into(), ..Default::default() };
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(launch, "/".into(), tx, Duration::ZERO);
        app.new_tab();

        typed(&mut app, "this is text sample");
        paste(&mut app);
        typed(&mut app, "And this is pasted image.");
        app.set_input(format!("{}\nIt should have preview", app.input));
        app.attach_bytes("gone.png".into(), vec![9], "image/png"); // pasted, then its token deleted below
        let stale = image::token(2);
        app.input = app.input.replace(&stale, "");
        app.drop_orphan_images();
        press(&mut app, KeyCode::Enter);
        drop(rx);

        let log = (0..60)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(50));
                std::fs::read_to_string(dir.join("stdin")).ok().filter(|t| t.contains("\"user\""))
            })
            .expect("the user message reached the process");
        let msg = log.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).find(|v| v["type"] == "user").unwrap();
        let kinds: Vec<String> = msg["message"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| if b["type"] == "text" { format!("text:{}", b["text"].as_str().unwrap().replace('\n', "|")) } else { b["type"].as_str().unwrap().to_string() })
            .collect();
        assert_eq!(kinds, ["text:this is text sample", "text:[Image #1]", "image", "text:And this is pasted image.|It should have preview"]);
        assert!(matches!(&app.sessions[0].items[0].entry, session::Entry::Rich(p) if p.len() == 3));
        assert_eq!(app.sessions[0].title, "this is text sample".chars().take(18).collect::<String>());
        assert!(app.sessions[0].attachments.is_empty());
        assert_eq!(app.history, ["this is text sample\nAnd this is pasted image.\nIt should have preview"], "history keeps words, not tokens");
    }

    #[test]
    fn previews_are_drawn_under_their_token_in_the_input_and_in_the_transcript() {
        use ratatui::{Terminal, backend::TestBackend, layout::Position};
        let (mut app, _d) = fake_app("prev");
        let shown = image::Shown { id: 0x0001_0203, cols: 6, rows: 3, w: 10, h: 5 };
        typed(&mut app, "look");
        paste(&mut app);
        typed(&mut app, "done");
        app.sessions[0].attachments[0].preview = Some(shown);

        let render = |app: &mut App| -> (Vec<String>, Position) {
            let mut t = Terminal::new(TestBackend::new(40, 24)).unwrap();
            t.draw(|f| ui::draw(f, app)).unwrap();
            let buf = t.backend().buffer().clone();
            let rows = (0..24).map(|y| (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect();
            (rows, t.get_cursor_position().unwrap())
        };
        let (rows, cursor) = render(&mut app);
        let at = rows.iter().position(|r| r.contains("[Image #1]")).expect("token line");
        assert!(rows[at + 1..=at + 3].iter().all(|r| r.contains('\u{10EEEE}')), "three preview rows follow the token: {rows:?}");
        assert!(rows[at + 4].contains("done"), "the text after the picture comes after its rows");
        assert_eq!(cursor.y as usize, at + 4, "cursor is on the text row below the picture");

        // after sending, the transcript holds the same order
        app.sessions[0].push(session::Entry::Rich(vec![
            session::Part::Text("look".into()),
            session::Part::Image(1, Some(shown)),
            session::Part::Text("done".into()),
        ]));
        app.set_input(String::new());
        let (rows, _) = render(&mut app);
        let look = rows.iter().position(|r| r.contains("> look")).unwrap();
        assert!(rows[look + 1].contains("[Image #1]") && rows[look + 2].contains('\u{10EEEE}') && rows[look + 4].contains('\u{10EEEE}'));
        assert!(rows[look + 5].contains("done"));
    }

    fn model(value: &str, resolved: &str, efforts: &[&str]) -> ModelInfo {
        // only the opus models have fast mode, as in real life
        ModelInfo { value: value.into(), name: value.to_uppercase(), desc: String::new(), resolved: resolved.into(), efforts: efforts.iter().map(|e| e.to_string()).collect(), fast: value.starts_with("opus") }
    }

    #[test]
    fn model_picker_applies_model_and_effort_and_remembers_them() {
        let (mut app, dir) = fake_app("picker");
        app.known_models = vec![model("sonnet", "claude-sonnet-5-5", &["low", "high"]), model("haiku", "claude-haiku-5-5", &["low", "medium", "high"])];
        app.sessions[0].model = "claude-sonnet-5-5".into();
        app.sessions[0].effort = "high".into();
        run_cmd(&mut app, "/model");
        let p = app.model_picker.unwrap();
        assert_eq!((p.sel, p.effort), (0, 1), "opens on the current model and effort");
        typed(&mut app, "zzz");
        assert!(app.input.is_empty(), "keys go to the picker");

        press(&mut app, KeyCode::Down); // haiku: "high" is kept by name (index 2)
        assert_eq!(app.model_picker.unwrap().effort, 2);
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Enter);
        assert!(app.model_picker.is_none());
        assert_eq!(app.sessions[0].model_pref.as_deref(), Some("haiku"));
        assert_eq!(app.sessions[0].effort_pref.as_deref(), Some("medium"));
        assert_eq!(spawns(&dir), 0, "choosing before the backend exists starts nothing");

        // Escape changes nothing; choosing what is already active is a no-op.
        run_cmd(&mut app, "/effort");
        press(&mut app, KeyCode::Esc);
        assert!(app.model_picker.is_none());
        run_cmd(&mut app, "/effort bogus");
        assert_eq!(app.sessions[0].effort_pref.as_deref(), Some("medium"), "a level the model lacks is refused");
    }

    #[test]
    fn current_model_prefers_the_pick_and_picker_fits_a_short_terminal() {
        let (mut app, _d) = fake_app("cur");
        app.known_models = vec![model("default", "claude-sonnet-5-5", &["low", "high"]), model("sonnet", "claude-sonnet-5-5", &["low"]), model("haiku", "claude-haiku-5-5", &["low"])];
        assert_eq!(app.current_model(), None);
        app.sessions[0].model = "claude-sonnet-5-5".into();
        assert_eq!(app.current_model(), Some(0), "first alias with that id");
        app.sessions[0].model_pref = Some("sonnet".into());
        assert_eq!(app.current_model(), Some(1), "the explicit pick wins");

        app.known_models.extend((0..20).map(|i| model(&format!("m{i}"), &format!("id{i}"), &["low", "high"])));
        run_cmd(&mut app, "/model");
        app.model_picker = Some(ModelPicker { sel: 22, effort: 1, fast: false });
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        t.draw(|f| ui::draw(f, &mut app)).unwrap();
        let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("M19") && text.contains("effort") && text.contains("enter apply"), "selected row and effort row both visible");
        assert!(text.contains("fast") && text.contains("not available"), "the fast row is there even for a model without it");
    }

    #[test]
    fn fast_mode_toggles_in_the_picker_for_opus_only_and_follows_model_switches() {
        let (mut app, _dir) = fake_app("fast");
        app.known_models = vec![model("sonnet", "claude-sonnet-5-5", &["low", "high"]), model("opus", "claude-opus-5-5", &["low", "high"])];
        app.sessions[0].model = "claude-sonnet-5-5".into();
        app.sessions[0].effort = "high".into();
        run_cmd(&mut app, "/model");
        press(&mut app, KeyCode::Char(' '));
        assert!(!app.model_picker.unwrap().fast, "sonnet has no fast mode: space does nothing");
        press(&mut app, KeyCode::Down); // opus
        press(&mut app, KeyCode::Char(' '));
        assert!(app.model_picker.unwrap().fast);
        press(&mut app, KeyCode::Char('f'));
        assert!(!app.model_picker.unwrap().fast, "f toggles too");
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        let s = &app.sessions[0];
        assert_eq!((s.model_pref.as_deref(), s.fast_pref), (Some("opus"), Some(true)));
        assert!(matches!(&s.items.last().unwrap().entry, session::Entry::Note(t) if t.contains("fast ↯") && t.contains("usage credits")), "the cost is stated");

        // the backend confirms: the state follows what it reports
        app.sessions[0].handle(&serde_json::from_str(r#"{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"fast_mode_state":"on"}}}"#).unwrap());
        assert!(app.sessions[0].fast_mode);

        // moving to a model without fast mode turns it off; the picker opens showing the current state
        run_cmd(&mut app, "/model");
        assert!(app.model_picker.unwrap().fast, "opens on the current fast state");
        press(&mut app, KeyCode::Up); // sonnet
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.sessions[0].fast_pref, Some(false));
        assert_eq!(app.sessions[0].model_pref.as_deref(), Some("sonnet"));
    }

    #[test]
    fn a_direct_model_switch_away_from_opus_turns_fast_mode_off() {
        let (mut app, _dir) = fake_app("fast2");
        app.sessions[0].models = vec![model("sonnet", "claude-sonnet-5-5", &["low"]), model("opus", "claude-opus-5-5", &["low"])];
        app.sessions[0].fast_mode = true;
        run_cmd(&mut app, "/model opus");
        assert_eq!(app.sessions[0].fast_pref, None, "still opus: untouched");
        run_cmd(&mut app, "/model sonnet");
        assert_eq!(app.sessions[0].fast_pref, Some(false));
    }

    #[test]
    fn the_suggestion_popup_uses_the_whole_screen_width() {
        use ratatui::{Terminal, backend::TestBackend};
        let (mut app, _d) = fake_app("wide");
        let long = "d".repeat(150);
        app.known_commands = vec![cmd("compact", "[focus]", &long)];
        typed(&mut app, "/comp");
        let mut row = |w: u16| -> String {
            let mut t = Terminal::new(TestBackend::new(w, 14)).unwrap();
            t.draw(|f| ui::draw(f, &mut app)).unwrap();
            let buf = t.backend().buffer().clone();
            (0..14)
                .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
                .find(|r| r.contains("/compact"))
                .unwrap()
        };
        let wide = row(160);
        assert!(wide.matches('d').count() > 120, "the description runs far past the old 90-column limit: {}", wide.matches('d').count());
        assert!(wide.contains('…') && wide.trim_end().ends_with('│'), "and is cut with an ellipsis inside the frame");
        let narrow = row(60);
        assert!(narrow.matches('d').count() < 40 && narrow.contains('…'), "still truncates on a small screen");

        // file paths too
        app.cwd = project("wide").to_string_lossy().into();
        app.set_input("@".into());
        app.refresh_files();
        let mut t = Terminal::new(TestBackend::new(160, 14)).unwrap();
        t.draw(|f| ui::draw(f, &mut app)).unwrap();
        let buf = t.backend().buffer().clone();
        let text: String = (0..14).flat_map(|y| (0..160).map(move |x| (x, y))).map(|(x, y)| buf[(x, y)].symbol().to_string()).collect();
        assert!(text.contains("@docs/") && text.contains("@README.md"));
    }

    #[test]
    fn model_picker_without_a_known_list_starts_a_backend_to_learn_it() {
        let (mut app, dir) = fake_app("picker2");
        run_cmd(&mut app, "/model");
        assert_eq!(wait_spawns(&dir, 1), 1);
        assert!(app.model_picker.is_some(), "it shows 'loading' until the list arrives");
    }

    #[test]
    fn model_cache_round_trips() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-models-{}", std::process::id()));
        let path = dir.join("m").join("models.json");
        write_models(&path, &[model("sonnet", "claude-sonnet-5-5", &["low", "max"])]);
        let back = read_models(&path);
        assert_eq!((back[0].value.as_str(), back[0].resolved.as_str(), back[0].efforts.clone()), ("sonnet", "claude-sonnet-5-5", vec!["low".to_string(), "max".to_string()]));
    }

    fn status_app(tag: &str, interval: Option<u64>) -> (App, mpsc::Receiver<AppEvent>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-sl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(Launch::default(), dir.to_string_lossy().into(), tx, Duration::ZERO);
        app.new_tab();
        let command = format!("cat > {d}/last.json; echo x >> {d}/runs; echo 'line one'; echo 'line two'", d = dir.display());
        app.statusline = Some(statusline::Config { command, interval: interval.map(Duration::from_secs), padding: 0, source: statusline::Source::Flag });
        app.statusline_on = true;
        (app, rx, dir)
    }
    fn runs(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("runs")).map_or(0, |t| t.lines().count())
    }
    fn settle(app: &mut App, rx: &mpsc::Receiver<AppEvent>) {
        let ev = rx.recv_timeout(Duration::from_secs(3)).expect("status line finished");
        app.on_event(ev);
    }

    #[test]
    fn the_status_line_refreshes_on_change_not_on_a_timer_and_never_while_idle() {
        let (mut app, rx, dir) = status_app("policy", Some(5));
        app.tick(); // never ran: runs once
        settle(&mut app, &rx);
        assert_eq!(app.sessions[0].status_lines.len(), 2);
        assert_eq!(app.sessions[0].status_lines[0].spans[0].content, "line one");
        let sent: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("last.json")).unwrap()).unwrap();
        assert_eq!((sent["cwd"].as_str(), sent["workspace"]["current_dir"].as_str()), (Some(app.cwd.as_str()), Some(app.cwd.as_str())));
        assert_eq!(runs(&dir), 1);

        for _ in 0..5 {
            app.tick();
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!((runs(&dir), rx.try_recv().is_err()), (1, true), "nothing changed, so nothing runs");

        // something changed: updates are spaced 300 ms apart (Claude Code's debounce)
        app.sessions[0].status_dirty = true;
        app.tick();
        assert_eq!(runs(&dir), 1, "too soon after the last start");
        std::thread::sleep(Duration::from_millis(350));
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 2);

        // the configured refreshInterval applies only while the user is around
        app.sessions[0].status_ran = Some(Instant::now() - Duration::from_secs(10));
        app.last_input = Instant::now() - Duration::from_secs(700);
        app.tick();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(runs(&dir), 2, "idle for ten minutes: the timer stops");
        app.last_input = Instant::now();
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 3);

        // switched off: no runs even when dirty
        app.statusline_on = false;
        app.sessions[0].status_dirty = true;
        app.sessions[0].status_ran = Some(Instant::now() - Duration::from_secs(10));
        app.tick();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(runs(&dir), 3);
    }

    #[test]
    fn a_newer_update_cancels_the_run_in_flight_and_only_its_result_counts() {
        let (mut app, rx, dir) = status_app("cancel", None);
        let slow = format!("n=$(cat {d}/n 2>/dev/null || echo 0); echo $((n+1)) > {d}/n; if [ $n = 0 ]; then sleep 5; echo OLD; else echo NEW; fi", d = dir.display());
        app.statusline.as_mut().unwrap().command = slow;
        app.tick(); // first run: slow
        std::thread::sleep(Duration::from_millis(400));
        app.sessions[0].status_dirty = true;
        app.tick(); // second run cancels the first
        settle(&mut app, &rx); // whichever arrives first...
        if app.sessions[0].status_lines.is_empty() {
            settle(&mut app, &rx);
        }
        let shown: String = app.sessions[0].status_lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(shown, "NEW", "the cancelled run's late result is ignored");
        assert!(!app.sessions[0].status_running);
    }

    #[test]
    fn documented_triggers_refresh_the_line_permission_mode_compact_and_reset_times() {
        let (mut app, rx, dir) = status_app("triggers", None);
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 1);
        let settle_after = |app: &mut App, ms: u64| {
            std::thread::sleep(Duration::from_millis(ms));
            app.tick();
        };
        // permission mode change (shift+tab)
        settle_after(&mut app, 350);
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 2, "permission mode change");
        // /compact finishing
        settle_after(&mut app, 350);
        app.sessions[0].handle(&serde_json::from_str(r#"{"type":"system","subtype":"compact_boundary"}"#).unwrap());
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 3, "/compact");
        // a limit window reaching its reset time, with no other event
        settle_after(&mut app, 350);
        app.sessions[0].status_next = Some(1.0); // long past
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 4, "reset time reached");
        settle_after(&mut app, 350);
        app.tick();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(runs(&dir), 4, "and it does not loop once the deadline is consumed");
    }

    #[test]
    fn switching_tabs_refreshes_the_new_tab_and_each_tab_keeps_its_own_lines() {
        let (mut app, rx, dir) = status_app("tabs", None);
        app.tick();
        settle(&mut app, &rx);
        app.new_tab();
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 2);
        assert!(app.sessions[0].status_lines.len() == 2 && app.sessions[1].status_lines.len() == 2);
        std::thread::sleep(Duration::from_millis(1100));
        app.switch(-1);
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(runs(&dir), 3, "coming back to a tab refreshes it (its countdowns may be stale)");
    }

    #[test]
    fn the_status_line_is_chosen_on_our_side_and_the_choice_is_remembered() {
        let (mut app, rx, dir) = status_app("mode", None);
        let claude_home = dir.join("claude");
        std::fs::create_dir_all(&claude_home).unwrap();
        std::fs::write(claude_home.join("settings.json"), r#"{"statusLine": {"type": "command", "command": "echo from-claude"}}"#).unwrap();
        app.claude_paths = statusline::Paths { home: Some(dir.clone()), config_dir: Some(claude_home.clone()), claude_json: None, managed: dir.join("none.json") };
        let ours = dir.join("cpp").join("config.json");
        app.config_path = Some(ours.clone());

        run_cmd(&mut app, "/statusline builtin");
        assert!(!app.statusline_on && app.statusline.is_none());
        assert_eq!(config::get_str(&ours, "statusline").as_deref(), Some("builtin"));
        app.tick();
        std::thread::sleep(Duration::from_millis(300));
        assert!(rx.try_recv().is_err() && runs(&dir) == 0, "builtin never starts a process");

        run_cmd(&mut app, "/statusline claude");
        assert!(app.statusline_on);
        let c = app.statusline.clone().unwrap();
        assert_eq!((c.command.as_str(), c.source), ("echo from-claude", statusline::Source::User));
        assert_eq!(config::get_str(&ours, "statusline").as_deref(), Some("claude"));
        app.tick();
        settle(&mut app, &rx);
        assert_eq!(app.sessions[0].status_lines[0].spans[0].content, "from-claude");

        run_cmd(&mut app, "/statusline");
        assert!(matches!(&app.sessions[0].items.last().unwrap().entry, session::Entry::Note(t) if t.contains("claude: running `echo from-claude`") && t.contains("builtin")));
        run_cmd(&mut app, "/statusline bogus");
        assert!(app.flash.as_deref().is_some_and(|f| f.contains("usage")) && app.statusline_on, "a bad word changes nothing");
        run_cmd(&mut app, "/statusline off"); // alias of builtin
        assert!(!app.statusline_on);

        // claude mode with nothing in Claude Code's settings: say so and show our own line
        std::fs::remove_file(claude_home.join("settings.json")).unwrap();
        run_cmd(&mut app, "/statusline claude");
        assert!(!app.statusline_on);
        assert!(matches!(&app.sessions[0].items.last().unwrap().entry, session::Entry::Note(t) if t.contains("no `statusLine`") && t.contains("own line")));
        assert_eq!(app.statusline_mode, statusline::Mode::Claude, "the choice stands; it applies once Claude Code has one");
    }

    #[test]
    fn the_users_lines_sit_under_the_input_and_ours_drops_what_they_already_show() {
        use ratatui::{Terminal, backend::TestBackend};
        let (mut app, _rx, _d) = status_app("draw", None);
        app.sessions[0].model = "claude-test".into();
        app.sessions[0].ctx_tokens = 5000;
        app.sessions[0].cost = 0.25;
        let rows = |app: &mut App| -> Vec<String> {
            let mut t = Terminal::new(TestBackend::new(70, 16)).unwrap();
            t.draw(|f| ui::draw(f, app)).unwrap();
            let buf = t.backend().buffer().clone();
            (0..16).map(|y| (0..70).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
        };
        let before = rows(&mut app);
        assert!(before[15].contains("claude-test") && before[15].contains("ctx 5.0k"), "no user lines yet: our full line");
        app.sessions[0].status_lines = vec![statusline::parse_line("\x1b[31mUSER ONE\x1b[0m"), statusline::parse_line("USER TWO")];
        let after = rows(&mut app);
        assert_eq!((after[13].as_str(), after[14].as_str()), ("USER ONE", "USER TWO"));
        assert!(!after[15].contains("claude-test") && !after[15].contains("ctx") && !after[15].contains("$0.250"), "{:?}", after[15]);
        assert!(after[15].contains("mem"), "the parts only we know remain");
        // like Claude Code, it steps aside for a permission prompt
        app.sessions[0].pending.push_back(session::Pending {
            request_id: "r".into(),
            tool: "Bash".into(),
            input: json!({"command": "ls"}),
            suggestions: json!([]),
            ui: Default::default(),
        });
        let during = rows(&mut app);
        assert!(!during.iter().any(|r| r.contains("USER ONE")), "hidden while a prompt is up");
        assert!(during[15].contains("claude-test"), "and our full line comes back with it");
        app.sessions[0].pending.clear();
        app.statusline_on = false;
        assert!(rows(&mut app)[15].contains("claude-test"), "toggled off: back to ours");
    }

    fn ask(app: &mut App, json_questions: &str) {
        let line = format!(r#"{{"type":"control_request","request_id":"q1","request":{{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{{"questions":{json_questions}}}}}}}"#);
        app.sessions[0].handle(&serde_json::from_str(&line).unwrap());
    }
    const QUESTIONS: &str = r#"[{"question":"Which?","header":"Pick","multiSelect":false,"options":[{"label":"A"},{"label":"B"}]},{"question":"Also?","header":"More","multiSelect":true,"options":[{"label":"X"},{"label":"Y"}]}]"#;

    /// An app whose `claude` logs everything it is sent, so what the panel answers can be read back.
    fn logging_app(tag: &str) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let (tx, rx) = mpsc::channel();
        std::mem::forget(rx);
        let mut app = App::new(Launch { bin: script.to_string_lossy().into(), ..Default::default() }, "/".into(), tx.clone(), Duration::ZERO);
        app.new_tab();
        let launch = app.launch_for(0);
        app.sessions[0].warm(&launch, &tx);
        (app, dir)
    }
    fn control_responses(dir: &std::path::Path, want: usize) -> Vec<serde_json::Value> {
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(50));
            let got: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("stdin"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|v| v["type"] == "control_response")
                .collect();
            if got.len() >= want {
                return got;
            }
        }
        panic!("no control_response arrived");
    }

    #[test]
    fn the_question_panel_owns_the_keyboard_and_its_answers_reach_the_agent() {
        let (mut app, dir) = logging_app("qpanel");
        ask(&mut app, QUESTIONS);
        // arrows and digits drive the panel
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter); // Which? = B, moves on
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('2')); // Also? = X, Y
        press(&mut app, KeyCode::Enter); // to review
        assert_eq!(app.sessions[0].pending.len(), 1, "not sent until you submit the review");
        press(&mut app, KeyCode::Enter);
        assert!(app.sessions[0].pending.is_empty());
        let r = &control_responses(&dir, 1)[0];
        let answers = &r["response"]["response"]["updatedInput"]["answers"];
        assert_eq!((answers["Which?"].as_str(), answers["Also?"].as_str()), (Some("B"), Some("X, Y")));
    }

    #[test]
    fn esc_declines_a_question_and_the_agent_hears_a_denial() {
        let (mut app, dir) = logging_app("qesc");
        ask(&mut app, QUESTIONS);
        typed(&mut app, "hi");
        assert!(app.input.is_empty(), "typing goes into the panel's Other field, not the message box");
        press(&mut app, KeyCode::Esc); // stops typing
        assert_eq!(app.sessions[0].pending.len(), 1);
        press(&mut app, KeyCode::Esc); // declines
        assert!(app.sessions[0].pending.is_empty());
        let r = &control_responses(&dir, 1)[0];
        assert_eq!(r["response"]["response"]["behavior"], "deny");
    }

    #[test]
    fn app_chords_and_scrolling_still_work_while_a_panel_is_up() {
        let (mut app, _dir) = logging_app("qchords");
        ask(&mut app, QUESTIONS);
        app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.sessions.len(), 2, "ctrl+t opens a tab even while a question waits in the other one");
        app.switch(-1);
        let before = app.sessions[0].scroll;
        press(&mut app, KeyCode::PageUp);
        assert_eq!(app.sessions[0].scroll, before + app.sessions[0].view_h - 2, "page keys scroll the transcript behind the panel");
        assert_eq!(app.sessions[0].pending.len(), 1, "and neither answered anything");
    }

    #[test]
    fn pasting_fills_other_for_a_question_and_does_nothing_for_a_permission_prompt() {
        let (mut app, dir) = logging_app("qpaste");
        ask(&mut app, r#"[{"question":"Which?","header":"Pick","multiSelect":false,"options":[{"label":"A"},{"label":"B"}]}]"#);
        app.on_paste("2 apples".into());
        assert!(app.input.is_empty());
        press(&mut app, KeyCode::Enter);
        let answers = control_responses(&dir, 1)[0]["response"]["response"]["updatedInput"]["answers"].clone();
        assert_eq!(answers["Which?"], "2 apples", "pasted '2' did not pick option 2");

        app.sessions[0].handle(&serde_json::from_str(r#"{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[]}}"#).unwrap());
        app.on_paste("y".into());
        assert_eq!(app.sessions[0].pending.len(), 1, "a pasted y never approves");
        press(&mut app, KeyCode::Enter);
        assert!(app.sessions[0].pending.is_empty());
        let r = &control_responses(&dir, 2)[1];
        assert_eq!((r["response"]["request_id"].as_str(), r["response"]["response"]["behavior"].as_str()), (Some("p1"), Some("allow")));
    }

    /// Render the whole screen as text rows.
    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| ui::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
    }

    /// The consistency rule itself: whatever the surface, its panel is anchored at the bottom directly above
    /// the input rule, has a titled frame, and ends in a key-hint footer.
    #[test]
    fn every_panel_sits_at_the_bottom_above_the_input_with_the_same_frame() {
        let (mut app, _d) = fake_app("uniform");
        app.cwd = project("uniform").to_string_lossy().into();
        app.known_commands = vec![cmd("compact", "[focus]", "summarize the conversation")];
        app.known_models = vec![model("sonnet", "claude-sonnet-5-5", &["low", "high"]), model("opus", "claude-opus-5-5", &["low", "high"])];
        app.config_path = Some(std::env::temp_dir().join(format!("claudeplusplus-uniform-cfg-{}", std::process::id())).join("config.json"));
        let mut cases: Vec<(&str, &str)> = vec![];

        // each surface is brought up the way a user would
        let mut check = |app: &mut App, name: &'static str, title: &str, hint: &str| {
            for width in [60u16, 100, 160] {
                let rows = screen(app, width, 30);
                let input_rule = rows.iter().rposition(|r| r.starts_with("───")).unwrap_or_else(|| panic!("{name}@{width}: no input rule\n{}", rows.join("\n")));
                let bottom = input_rule - 1;
                assert!(rows[bottom].starts_with('└'), "{name}@{width}: the frame must end directly above the input, got {:?}\n{}", rows[bottom], rows.join("\n"));
                let top = rows[..bottom].iter().rposition(|r| r.starts_with('┌')).unwrap_or_else(|| panic!("{name}@{width}: no top border"));
                assert!(rows[top].starts_with(&format!("┌ {title}")), "{name}@{width}: title {:?} vs {:?}", title, rows[top]);
                assert!(rows[bottom - 1].contains(hint), "{name}@{width}: footer {:?} should contain {hint:?}", rows[bottom - 1]);
                assert!(rows[bottom - 1].starts_with("│ ") && rows[bottom - 1].ends_with('│'), "{name}@{width}: footer sits inside the frame");
                assert!(rows.iter().all(|r| r.chars().count() <= width as usize), "{name}@{width}: nothing wider than the screen");
            }
            cases.push((name, "ok"));
        };

        typed(&mut app, "/comp");
        check(&mut app, "command suggestions", "Commands", "tab complete");
        app.set_input("@".into());
        app.refresh_files();
        check(&mut app, "file suggestions", "Files", "tab complete");
        app.set_input(String::new());
        app.refresh_files();

        run_cmd(&mut app, "/model");
        check(&mut app, "model picker", "Model, effort & fast mode", "esc cancel");
        press(&mut app, KeyCode::Esc);

        run_cmd(&mut app, "/config");
        check(&mut app, "config", "Config", "esc close");
        press(&mut app, KeyCode::Esc);

        run_cmd(&mut app, "/session");
        check(&mut app, "session config", "Session", "esc close");
        press(&mut app, KeyCode::Esc);

        app.explorer = Some(explorer::Explorer::for_bench(vec![history::Summary { id: "abcdef12-0000".into(), title: "a session".into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 10 }]));
        let rows = screen(&mut app, 100, 30);
        assert!(rows[0].starts_with(" Sessions 1/1"), "title on the first row: {:?}", rows[0]);
        assert!(rows[29].contains("esc/→ close"), "hints on the last row: {:?}", rows[29]);
        assert!(!rows.iter().any(|r| r.contains('┌') || r.contains('└') || r.starts_with("───")), "full screen has no frame, tabs or input\n{}", rows.join("\n"));
        app.explorer = None;

        ask(&mut app, QUESTIONS);
        check(&mut app, "question", "Question 1/2", "esc decline");
        press(&mut app, KeyCode::Esc);
        app.sessions[0].handle(&serde_json::from_str(r#"{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[]}}"#).unwrap());
        check(&mut app, "permission", "Permission", "esc deny");
        assert_eq!(cases.len(), 7, "all seven panels were checked (the session list is full screen, checked above)");
    }

    #[test]
    fn blocking_panels_are_yellow_and_the_ones_you_open_are_cyan() {
        use ratatui::style::Color;
        use ratatui::{Terminal, backend::TestBackend};
        let (mut app, _d) = fake_app("frames");
        let border = |app: &mut App| -> Option<Color> {
            let mut t = Terminal::new(TestBackend::new(60, 20)).unwrap();
            t.draw(|f| ui::draw(f, app)).unwrap();
            let buf = t.backend().buffer().clone();
            (0..20).flat_map(|y| (0..60).map(move |x| (x, y))).find(|&(x, y)| buf[(x, y)].symbol() == "┌").map(|(x, y)| buf[(x, y)].fg)
        };
        assert_eq!(border(&mut app), None);
        run_cmd(&mut app, "/config");
        assert_eq!(border(&mut app), theme::accent().fg, "you opened it: cyan");
        press(&mut app, KeyCode::Esc);
        ask(&mut app, QUESTIONS);
        assert_eq!(border(&mut app), theme::attention().fg, "the agent is waiting: yellow");
    }

    #[test]
    fn config_applies_at_once_persists_and_keeps_the_keyboard() {
        let (mut app, _d) = fake_app("settings");
        let cfg = std::env::temp_dir().join(format!("claudeplusplus-settings-cfg-{}", std::process::id())).join("config.json");
        let _ = std::fs::remove_file(&cfg);
        app.config_path = Some(cfg.clone());
        run_cmd(&mut app, "/config");
        assert!(app.settings.is_some());
        typed(&mut app, "abc");
        assert!(app.input.is_empty(), "keys belong to the panel");
        app.on_paste("pasted".into());
        assert!(app.input.is_empty());

        // row 1: profile for new tabs
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.default_profile, "sources");
        assert_eq!(config::get_str(&cfg, "profile").as_deref(), Some("sources"), "saved the moment it changed");
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.default_profile, "full", "space and enter step forward, wrapping");
        press(&mut app, KeyCode::Left);
        assert_eq!(app.default_profile, "lean", "left steps back, wrapping");
        app.new_tab();
        assert_eq!(app.sessions.last().unwrap().profile, "lean", "new tabs use it");

        // row 3: cache lifetime reaches every tab; row 4: idle suspend; row 5: memory budget
        for _ in 0..2 {
            press(&mut app, KeyCode::Down);
        }
        press(&mut app, KeyCode::Left); // 60 -> 5 min
        assert_eq!(app.cache_ttl, Duration::from_secs(300));
        assert!(app.sessions.iter().all(|s| s.cache_ttl == Duration::from_secs(300)));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right); // never (the fake app's value) -> 5 min
        assert_eq!(app.idle_suspend, Duration::from_secs(300));
        assert_eq!(config::get_str(&cfg, "idle_suspend_s").as_deref(), Some("300"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Right); // off -> 500 MB
        assert_eq!(app.mem_budget_kb, 500 * 1024);
        assert_eq!(config::get_str(&cfg, "mem_budget_mb").as_deref(), Some("500"));

        // row 2: images (a process-wide mode; put it back so other tests are unaffected)
        for _ in 0..3 {
            press(&mut app, KeyCode::Up);
        }
        press(&mut app, KeyCode::Right);
        assert_eq!(image::mode(), "on");
        press(&mut app, KeyCode::Right);
        press(&mut app, KeyCode::Right);
        assert_eq!(image::mode(), "auto");

        press(&mut app, KeyCode::Esc);
        assert!(app.settings.is_none());
        assert_eq!(app.input, "", "nothing leaked into the message box");
    }

    #[test]
    fn the_status_line_config_switches_modes_and_explains_when_claude_has_none() {
        let (mut app, _d) = fake_app("settings-sl");
        let dir = std::env::temp_dir().join(format!("claudeplusplus-settings-sl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        app.claude_paths = statusline::Paths { home: Some(dir.clone()), config_dir: Some(dir.join("nothing")), claude_json: None, managed: dir.join("none.json") };
        app.config_path = Some(dir.join("config.json"));
        assert_eq!(app.setting_value("statusline"), "builtin");
        run_cmd(&mut app, "/config");
        press(&mut app, KeyCode::Right); // builtin -> claude, but Claude Code has no status line
        assert_eq!(app.setting_value("statusline"), "claude", "the choice stands");
        assert!(!app.statusline_on);
        assert!(app.flash.as_deref().is_some_and(|f| f.contains("no `statusLine`") && f.contains("own line")), "{:?}", app.flash);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.setting_value("statusline"), "builtin");
    }

    #[test]
    fn left_arrow_at_the_start_of_the_input_opens_the_session_list_else_moves_the_cursor() {
        let (mut app, _d) = fake_app("leftkey");
        typed(&mut app, "ab");
        press(&mut app, KeyCode::Left);
        assert!(app.explorer.is_none() && app.cursor == 1, "text to the left: just a cursor move");
        press(&mut app, KeyCode::Left);
        assert!(app.explorer.is_none() && app.cursor == 0);
        press(&mut app, KeyCode::Left);
        assert!(app.explorer.is_some(), "nothing to the left of the cursor opens the list");
        press(&mut app, KeyCode::Esc);
        assert!(app.explorer.is_none() && app.input == "ab", "closing leaves the draft alone");
    }

    #[test]
    fn fork_opens_its_own_tab_resumed_as_a_copy_and_stop_runs_claude_stop() {
        let (mut app, dir) = fake_app("fork");
        app.open_session("aaaa1111".into(), "t".into(), None);
        app.explorer_outcome(explorer::Outcome::Fork("aaaa1111".into(), "t".into(), None));
        assert_eq!(app.sessions.len(), 2, "a copy gets its own tab even when the original is open");
        assert!(app.sessions[1].fork && !app.sessions[0].fork);
        let (launch, tx) = (app.launch_for(1), app.tx.clone());
        app.sessions[1].warm(&launch, &tx);
        app.explorer_outcome(explorer::Outcome::Stop("ab12cd34".into(), None));
        let spawns = (0..60)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(50));
                std::fs::read_to_string(dir.join("spawns")).ok().filter(|t| t.contains("--fork-session") && t.contains("stop ab12cd34"))
            })
            .expect("fork flag on the resume, and `claude stop` ran");
        assert!(spawns.contains("--resume aaaa1111"), "{spawns}");
    }

    #[test]
    fn custom_profiles_reach_the_launch_and_the_dock_keeps_the_keyboard_after_opening() {
        let (mut app, _d) = fake_app("custprof");
        assert!(app.profile_flag_error("no-such-profile-xyz"));
        app.apply_setting("side_panel", "on");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Enter); // opens whatever row exists, if any
        assert!(app.dock_focus, "the docked list stays focused after opening a session");
    }

    #[test]
    fn scrolling_by_wheel_and_page_keeps_its_place_as_output_arrives_and_can_jump_back() {
        use ratatui::{Terminal, backend::TestBackend, crossterm::event::{MouseEvent, MouseEventKind}};
        let (mut app, _d) = fake_app("scroll");
        for i in 0..100 {
            app.s().note(format!("line {i}"));
        }
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut shown = |app: &mut App| {
            t.draw(|f| ui::draw(f, app)).unwrap();
            let b = t.backend().buffer().clone();
            (0..24).map(|y| (0..80).map(|x| b[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
        };
        assert!(shown(&mut app).contains("line 99") && !shown(&mut app).contains("lines below"));
        let wheel = |app: &mut App, kind| app.on_event(AppEvent::Term(Event::Mouse(MouseEvent { kind, column: 5, row: 5, modifiers: KeyModifiers::NONE })));
        wheel(&mut app, MouseEventKind::ScrollUp);
        assert_eq!(app.sessions[0].scroll, 3);
        let screen = shown(&mut app);
        assert!(screen.contains("↓ 3 lines below") && !screen.contains("line 99"));
        press(&mut app, KeyCode::PageUp);
        let page = app.sessions[0].view_h - 2;
        assert_eq!(app.sessions[0].scroll, 3 + page, "a page is the viewport minus two lines of overlap");
        let top = shown(&mut app).lines().find(|l| l.contains("line ")).unwrap().trim().to_string();
        app.s().note("new output".into());
        let after = shown(&mut app).lines().find(|l| l.contains("line ")).unwrap().trim().to_string();
        assert_eq!(top, after, "the lines in view do not move while new output arrives below");
        wheel(&mut app, MouseEventKind::ScrollDown);
        app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(app.sessions[0].scroll, 0);
        assert!(shown(&mut app).contains("new output"));
        app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert!(shown(&mut app).contains("line 0"), "ctrl+home reaches the top");
        app.apply_setting("mouse", "off");
        assert!(!app.mouse && app.setting_value("mouse") == "off");
    }

    #[test]
    fn the_dock_takes_a_left_column_only_on_wide_screens() {
        use ratatui::{Terminal, backend::TestBackend};
        let (mut app, _d) = fake_app("dockdraw");
        app.apply_setting("side_panel", "on");
        app.show_dock(false);
        for (w, docked) in [(80u16, false), (120, true)] {
            let mut t = Terminal::new(TestBackend::new(w, 24)).unwrap();
            t.draw(|f| ui::draw(f, &mut app)).unwrap();
            let buf = t.backend().buffer().clone();
            let row = |y: u16| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
            assert_eq!(row(0).contains("Sessions"), docked, "{w}");
            if docked {
                assert!(row(23).trim_start().starts_with('│') || row(23).contains('│'), "dock separator spans the height");
            }
        }
    }

    #[test]
    fn the_session_panel_sets_mode_and_tool_rules_and_takes_a_typed_rule() {
        let (mut app, _d) = fake_app("tabcfg");
        run_cmd(&mut app, "/session");
        assert!(app.tabcfg.is_some());
        typed(&mut app, "zz");
        assert!(app.input.is_empty(), "keys belong to the panel");
        assert!(screen(&mut app, 100, 30).iter().any(|r| r.starts_with("┌ Session")));
        app.sessions[0].mode = "default".into();
        press(&mut app, KeyCode::Right);
        assert_eq!(app.sessions[0].mode, "acceptEdits", "row 0 is the permission mode");
        press(&mut app, KeyCode::Down); // Bash
        press(&mut app, KeyCode::Right);
        assert_eq!(app.sessions[0].rule("Bash"), Some(true));
        press(&mut app, KeyCode::Right);
        assert_eq!(app.sessions[0].rule("Bash"), Some(false));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.sessions[0].rule("Bash"), None, "d removes the rule");
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up); // wraps to "add a rule"
        press(&mut app, KeyCode::Enter);
        typed(&mut app, "Bash(git push:*)");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.sessions[0].rule("Bash(git push:*)"), Some(true), "typed rules start as allow");
        assert!(tabcfg::rows(&app.sessions[0]).contains(&Row::Tool("Bash(git push:*)".into(), true)));
        // the cursor is on the rule just added
        press(&mut app, KeyCode::Right);
        assert_eq!(app.sessions[0].rule("Bash(git push:*)"), Some(false));
        press(&mut app, KeyCode::Right); // past deny removes a custom rule
        assert!(app.sessions[0].rules.is_empty());
        press(&mut app, KeyCode::Esc);
        assert!(app.tabcfg.is_none());
    }

    #[test]
    fn right_arrow_closes_the_dock_and_steps_back_out_of_the_full_screen_list() {
        let (mut app, _d) = fake_app("rightkey");
        app.apply_setting("side_panel", "on");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Left);
        assert!(app.explorer.is_some());
        press(&mut app, KeyCode::Right);
        assert!(app.explorer.is_none() && app.dock.is_some() && app.dock_focus, "→ leaves full screen for the dock");
        press(&mut app, KeyCode::Right);
        assert!(app.dock.is_none(), "→ closes the dock");
        app.apply_setting("side_panel", "off");
        press(&mut app, KeyCode::Left);
        assert!(app.explorer.is_some() && app.dock.is_none(), "full screen is always available");
        press(&mut app, KeyCode::Right);
        assert!(app.explorer.is_none() && app.dock.is_none(), "→ closes it");
    }

    #[test]
    fn in_panel_mode_left_docks_the_list_then_goes_full_screen_and_esc_steps_back() {
        let (mut app, _d) = fake_app("dock");
        app.apply_setting("side_panel", "on");
        press(&mut app, KeyCode::Left);
        assert!(app.dock.is_some() && app.dock_focus && app.explorer.is_none(), "first ← docks and focuses");
        typed(&mut app, "hi");
        assert!(!app.dock_focus && app.input == "hi", "typing returns to the input and the dock stays");
        assert!(app.dock.is_some());
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Left);
        assert!(app.dock_focus, "← at the start refocuses it");
        press(&mut app, KeyCode::Left);
        assert!(app.explorer.is_some(), "second ← goes full screen");
        press(&mut app, KeyCode::Esc);
        assert!(app.explorer.is_none() && app.dock_focus, "Esc steps back to the focused dock");
        press(&mut app, KeyCode::Esc);
        assert!(app.dock.is_some() && !app.dock_focus, "Esc unfocuses, still docked");
        app.toggle_sessions();
        assert!(app.dock.is_none(), "ctrl+r hides it");
        app.apply_setting("side_panel", "off");
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Left);
        assert!(app.explorer.is_some() && app.dock.is_none(), "full mode goes straight to the full screen");
    }

    #[test]
    fn the_explorer_takes_keys_and_reopens_into_a_blank_tab_else_a_new_one() {
        let (mut app, _d) = fake_app("explore");
        app.explorer = Some(explorer::Explorer::new("/nonexistent-dir", vec![]));
        typed(&mut app, "abc");
        assert!(app.input.is_empty(), "keys go to the explorer, not the message box");
        press(&mut app, KeyCode::Esc);
        assert!(app.explorer.is_none());

        app.open_session("11111111-aaaa".into(), "First chat title that is long".into(), None);
        assert_eq!(app.sessions.len(), 1, "blank tab is reused");
        assert_eq!(app.sessions[0].title, "First chat title t");
        app.open_session("22222222-bbbb".into(), "Second".into(), None);
        assert_eq!(app.sessions.len(), 2, "a used tab is kept");
        assert_eq!(app.active, 1);
    }

    /// Prints redraw cost with a 3000-entry transcript (run with `cargo test --release -- --nocapture`).
    /// Frame cost of every surface at a large terminal size. Run with
    /// `cargo test --release -- --ignored --nocapture frame_budget` and compare to DESIGN.md's budget.
    #[test]
    #[ignore]
    fn frame_budget() {
        use ratatui::{Terminal, backend::TestBackend};
        let (mut app, _d) = fake_app("budget");
        for i in 0..3000 {
            app.sessions[0].push(session::Entry::User(format!("question {i}")));
            app.sessions[0].push(session::Entry::Assistant {
                text: format!("# Heading {i}\n\nSome **bold** text with `code` and a [link](https://x.dev), then a list:\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |"),
                msg_id: String::new(),
                streaming: false,
            });
        }
        app.known_commands = (0..60).map(|i| cmd(&format!("command-{i}"), "[arg]", &"describes what it does ".repeat(6))).collect();
        app.known_models = (0..13).map(|i| model(&format!("m{i}"), &format!("id{i}"), &["low", "medium", "high"])).collect();
        let mut term = Terminal::new(TestBackend::new(200, 50)).unwrap();
        let mut measure = |label: &str, app: &mut App| {
            term.draw(|f| ui::draw(f, app)).unwrap(); // warm the wrap caches
            let n = 300;
            let t = Instant::now();
            for _ in 0..n {
                term.draw(|f| ui::draw(f, app)).unwrap();
            }
            println!("{label:<34} {:>7.0} µs/frame", t.elapsed().as_secs_f64() * 1e6 / n as f64);
        };
        measure("transcript only (3000 turns)", &mut app);
        app.set_input("/c".into());
        measure("+ command popup (60 commands)", &mut app);
        app.set_input("a long message ".repeat(30));
        measure("+ 7-line input", &mut app);
        app.set_input(String::new());
        app.model_picker = Some(ModelPicker { sel: 3, effort: 1, fast: false });
        measure("+ model picker", &mut app);
        app.model_picker = None;
        let sums: Vec<_> = (0..200)
            .map(|i| history::Summary { id: format!("{i:08}-aaaa"), title: format!("session {i}"), prompt: "first prompt".into(), modified: std::time::SystemTime::now(), size: 4096 })
            .collect();
        app.explorer = Some(explorer::Explorer::for_bench(sums));
        measure("+ session explorer (200)", &mut app);
        app.explorer = None;
        app.sessions[0].pending.push_back(session::Pending {
            request_id: "q".into(),
            tool: "AskUserQuestion".into(),
            input: json!({"questions": [{"question": "Which approach?", "header": "Approach", "multiSelect": false, "options": [
                {"label": "A", "description": "first way"}, {"label": "B", "description": "second way"}, {"label": "C", "description": "third way"}]}]}),
            suggestions: json!([]),
            ui: Default::default(),
        });
        measure("+ question panel", &mut app);
    }

    #[test]
    fn redraw_cost_stays_flat_on_a_long_transcript() {
        use ratatui::{Terminal, backend::TestBackend};
        let (tx, _rx) = mpsc::channel();
        std::mem::forget(_rx);
        let mut app = App::new(Launch::default(), "/".into(), tx, Duration::ZERO);
        app.new_tab();
        let text = "lorem ipsum dolor sit amet ".repeat(20);
        for i in 0..1500 {
            app.sessions[0].push(session::Entry::User(format!("question {i}")));
            app.sessions[0].push(session::Entry::Assistant { text: text.clone(), msg_id: String::new(), streaming: false });
        }
        app.sessions[0].push(session::Entry::Assistant { text: String::new(), msg_id: "m".into(), streaming: true });
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let t = Instant::now();
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        let first = t.elapsed();
        let n = 200;
        let t = Instant::now();
        for _ in 0..n {
            term.draw(|f| ui::draw(f, &mut app)).unwrap();
        }
        let steady = t.elapsed() / n;
        // Streaming: each delta invalidates and re-wraps only the last entry.
        let t = Instant::now();
        for i in 0..n {
            let item = app.sessions[0].items.last_mut().unwrap();
            if let session::Entry::Assistant { text, .. } = &mut item.entry {
                text.push_str("token ");
            }
            item.cache = None;
            term.draw(|f| ui::draw(f, &mut app)).unwrap();
            let _ = i;
        }
        let streaming = t.elapsed() / n;
        eprintln!("3000 entries: first draw {first:?}, steady redraw {steady:?}, streaming redraw {streaming:?}");
        assert!(steady < Duration::from_millis(50) && streaming < Duration::from_millis(50));
    }
}
