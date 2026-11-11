//! The supervisor: one small process per user that every running claude++ connects to (`claude++ supervisor`).
//!
//! What it does: it knows which claude++ instances are alive and what each one's tabs are doing, tells every
//! instance about the others, and keeps a ledger of the claude processes each instance started. If an instance
//! disappears without saying goodbye (killed, crashed), the processes in its ledger that are still running are
//! stopped: they belong to nobody now.
//!
//! It measures what every window and tab holds, and decides who stops: an idle tab past the idle limit, and,
//! when the windows together hold more than the memory budget, the longest-idle stoppable tab (it resumes on the
//! next message). The budget and the idle limit are its own settings, one value for all windows (`/config`, or
//! `--mem-budget` / `--idle-suspend` for this run). The owning window makes the final call: it refuses a tab you
//! have just switched to or that started working.
//!
//! What it never does: touch a process that claude++ did not start. Interactive `claude`, Claude Code's own
//! daemon and anything else are only listed (`/procs`). A process is stopped only when all of this holds: an
//! instance reported it as its own, its owner is gone for good (same pid *and* start time no longer running),
//! and the pid still has the start time it had when it was reported (so a reused pid is left alone).
//!
//! Managing it (`/supervisor`): a window can ask for its state (`status`), change its three settings (`set`), restart a
//! shared MCP server, or ask it to leave (`shutdown`). Windows reconnect, and start a new one, within ~10 s, unless
//! it was stopped on purpose: that leaves a marker in its directory until it is started again.
//!
//! Lifetime: it starts when the first instance needs it and ends `grace` after the last one has left. The
//! frontend works without it; a missing or refusing supervisor changes nothing on screen.
//!
//! Wire format: one JSON object per line over a Unix socket (mode 0600, in a directory only you can enter).
//!   client → `{"t":"hello","v":1,"pid":N}`, `{"t":"status"}`, `{"t":"set","key":"grace_s|cleanup|budget|budget_mb|idle_s","value":".."}`,
//!            `{"t":"mcp_restart","key":".."}`, `{"t":"shutdown"}`, `{"t":"tabs","tabs":[{..}],"owned":[pids],"win":{"focus":..,"input_idle_s":N}}`, `{"t":"bye"}`
//!   server → `{"t":"welcome","v":1,"instances":[..],"features":["mcp","manage","measure"],"pid":N}`, `{"t":"status",..}`, `{"t":"peers","instances":[..]}`,
//!            `{"t":"refuse","why":".."}`, `{"t":"suspend","tab":N,"why":".."}`
//! Shared MCP servers (`share_mcp`): a window may hand one JSON-RPC message of such a server to the supervisor, which
//! runs the process once for all windows (`share.rs`) and answers with the reply. The process ends when the last
//! window that used it is gone.
//!   client → `{"t":"mcp","id":N,"client":N,"def":{command,args,env,cwd},"msg":{..}}`   server → `{"t":"mcp_reply","id":N,"resp":{..}}`
//! An instance is `{"pid":N,"tabs":[{"id":1,"pid":backend|null,"busy":false,"mb":190,"cwd":"..","idle_s":0,"safe":true}]}`:
//! `idle_s` is in 30 s steps (so an idle tab does not make a message every second) and `safe` means the owner
//! could stop it now.

use crate::sync::LockOk;
use serde_json::{Value, json};
use crate::share::{self, Def, Reply};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const VERSION: u64 = 1;

#[derive(Clone)]
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    /// `/tmp/claudepp-<uid>`: short (a Unix socket path is limited to ~100 bytes) and the same from every terminal.
    pub fn user() -> Paths {
        // `CLAUDEPP_SUP_DIR`: a separate supervisor (benches and tests must not talk to the one you are using).
        if let Some(d) = std::env::var_os("CLAUDEPP_SUP_DIR").filter(|d| !d.is_empty()) {
            return Paths { dir: PathBuf::from(d) };
        }
        Paths { dir: PathBuf::from(format!("/tmp/claudepp-{}", unsafe { libc::getuid() })) }
    }
    pub fn sock(&self) -> PathBuf {
        self.dir.join("sup.sock")
    }
    /// Present while it is stopped on purpose: nobody starts it until `/supervisor` does.
    pub fn stopped_marker(&self) -> PathBuf {
        self.dir.join("stopped")
    }
    fn log_file(&self) -> PathBuf {
        self.dir.join("supervisor.log")
    }
    fn log(&self, msg: &str) {
        let f = self.log_file();
        if fs::metadata(&f).is_ok_and(|m| m.len() > 128 * 1024) {
            let _ = fs::remove_file(&f);
        }
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        if let Ok(mut o) = fs::OpenOptions::new().create(true).append(true).open(f) {
            let _ = writeln!(o, "{secs} {msg}");
        }
    }
    /// The directory exists, is yours and only yours.
    fn prepare(&self) -> io::Result<()> {
        match fs::create_dir(&self.dir) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        let m = fs::symlink_metadata(&self.dir)?;
        if !m.is_dir() || m.uid() != unsafe { libc::getuid() } {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the supervisor directory is not yours"));
        }
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
    }
}

#[derive(Clone, Copy)]
pub struct Config {
    /// How long it stays after the last instance has left.
    pub grace: Duration,
    /// After an instance vanished: how long before its processes are looked at (it may be restarting).
    pub reap_after: Duration,
    /// SIGTERM first; SIGKILL if the process is still there after this.
    pub kill_after: Duration,
    /// How often the memory budget is checked; one tab is asked to stop per check, so the next sample shows the effect.
    pub budget_every: Duration,
    /// Stop the processes a window that vanished left behind.
    pub cleanup: bool,
    /// Apply the memory budget.
    pub enforce: bool,
    /// Measure what the windows' processes hold (the tests of the budget say their own numbers instead).
    pub measure: bool,
    /// The memory budget for all windows together, in MB (0: none), and how long a tab may idle before it is
    /// stopped, in seconds (0: never). Settings of the supervisor, not of a window: one value for everyone.
    pub budget_mb: u64,
    pub idle_s: u64,
}

/// How long it stays after the last window, as `/config`-style values: seconds, or `always`.
pub fn parse_grace(v: &str) -> Option<Duration> {
    match v {
        "always" => Some(Duration::from_secs(10 * 365 * 86400)),
        n => n.parse().ok().map(Duration::from_secs),
    }
}

impl Config {
    /// The defaults, with what `/supervisor` saved in `config.json`.
    pub fn user() -> Config {
        let mut c = Config::default();
        if let Some(p) = crate::config::path() {
            let get = |k: &str| crate::config::get_str(&p, k);
            c.grace = get("supervisor_grace_s").and_then(|v| parse_grace(&v)).unwrap_or(c.grace);
            c.cleanup = get("supervisor_cleanup").as_deref() != Some("off");
            c.enforce = get("supervisor_budget").as_deref() != Some("paused");
            // The same keys `/config` saves them under ("Memory budget", "Stop idle tabs after").
            c.budget_mb = get("mem_budget_mb").and_then(|v| v.parse().ok()).unwrap_or(c.budget_mb);
            c.idle_s = get("idle_suspend_s").and_then(|v| v.parse().ok()).unwrap_or(c.idle_s);
        }
        c
    }
}

impl Default for Config {
    fn default() -> Config {
        Config { grace: Duration::from_secs(20), reap_after: Duration::from_secs(10), kill_after: Duration::from_secs(3), budget_every: Duration::from_secs(6), cleanup: true, enforce: true, measure: true, budget_mb: 0, idle_s: 120 }
    }
}

/// When the process with this pid started: with the pid it tells one process from a later one that got the same
/// pid. `None` when there is no such process.
#[cfg(target_os = "macos")]
pub fn stamp(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is a zeroed struct of exactly `size` bytes, which is all proc_pidinfo writes.
    let n = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, &mut info as *mut _ as *mut libc::c_void, size) };
    (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(target_os = "macos"))]
pub fn stamp(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (name) state ppid ...`: the name may hold spaces and parentheses, so count from the last `)`.
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

fn same(pid: u32, st: u64) -> bool {
    stamp(pid) == Some(st)
}

struct Inst {
    pid: u32,
    /// `None`: the start time could not be read, so this instance is never treated as gone.
    stamp: Option<u64>,
    tabs: Vec<Value>,
    owned: Vec<(u32, u64)>,
    bye: bool,
    /// What the window itself holds, as measured.
    ui_mb: u64,
    /// What the window says about itself: `{"focus": true|false|null, "input_idle_s": N}`. Null from an older build.
    win: Value,
    out: Out,
}

/// The write side of a connection, shared by the thread that reads it and the ones that answer on it.
type Out = Arc<Mutex<UnixStream>>;

#[derive(Default)]
struct State {
    insts: BTreeMap<u64, Inst>,
    next: u64,
    /// Vanished instances whose processes are still to be looked at: the supervisor stays for them.
    reaping: usize,
    /// The live settings (they start as the `Config` and `/supervisor` changes them).
    grace: Duration,
    cleanup: bool,
    enforce: bool,
    /// The memory budget (MB; 0: none) and the idle limit (s; 0: never), for all windows.
    budget_mb: u64,
    idle_s: u64,
    /// A window asked it to leave.
    quit: bool,
    started: Option<Instant>,
    /// Since it started: tabs the budget asked to stop, processes cleaned up after crashed windows, MCP messages relayed.
    suspended: u64,
    /// When each (window, tab) was last asked to stop: an owner that does not answer is not asked every check.
    asked: HashMap<(u64, u64), Instant>,
    reaped: u64,
    mcp_msgs: u64,
    /// The modification time of the binary it was started from, so a window can tell it is out of date.
    exe_mtime: u64,
}

impl State {
    fn new(cfg: &Config) -> State {
        State { grace: cfg.grace, cleanup: cfg.cleanup, enforce: cfg.enforce, budget_mb: cfg.budget_mb, idle_s: cfg.idle_s, started: Some(Instant::now()), exe_mtime: exe_mtime(), ..State::default() }
    }
}

/// The supervisor's own clocks.
struct Pace {
    budget: Instant,
    measure: Instant,
    /// When the windows were last told that the budget is exceeded and nothing can be stopped.
    noticed: Option<Instant>,
    empty_since: Option<Instant>,
}

impl Pace {
    fn new() -> Pace {
        Pace { budget: Instant::now(), measure: Instant::now() - Duration::from_secs(3600), noticed: None, empty_since: Some(Instant::now()) }
    }
}

fn mtime(p: &std::path::Path) -> u64 {
    fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

/// The binary this process runs, as `mtime` of the file; windows compare it with their own.
pub fn exe_mtime() -> u64 {
    std::env::current_exe().map_or(0, |p| mtime(&p))
}

/// The shared MCP servers with what they hold. Asks `ps`, so not under the state lock.
fn mcp_rows() -> Vec<Value> {
    let running = share::running();
    let pids: Vec<u32> = running.iter().filter_map(|r| r.pid).collect();
    let held = crate::memory::held_kb(&pids).0;
    let mb = |pid: Option<u32>| pid.and_then(|p| held.iter().find(|r| r.0 == p)).map_or(0, |r| r.1 / 1024);
    running.into_iter().map(|r| json!({"key": r.key, "command": r.command, "cwd": r.cwd, "pid": r.pid, "windows": r.windows, "starts": r.starts, "mb": mb(r.pid)})).collect()
}

/// Everything `/supervisor` shows.
fn status(s: &State, paths: &Paths, mcp: Vec<Value>) -> Value {
    let mb = |t: &Value| t["mb"].as_u64().unwrap_or(0);
    let windows: Vec<Value> = s.insts.values().map(|i| json!({"pid": i.pid, "tabs": i.tabs, "owned": i.owned.len(), "mb": i.ui_mb + i.tabs.iter().map(mb).sum::<u64>()})).collect();
    let total: u64 = windows.iter().map(|w| w["mb"].as_u64().unwrap_or(0)).sum();
    let log = fs::read_to_string(paths.log_file()).unwrap_or_default();
    let log: Vec<&str> = log.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    json!({
        "t": "status", "pid": std::process::id(), "v": VERSION, "exe_mtime": s.exe_mtime,
        "up_s": s.started.map_or(0, |t| t.elapsed().as_secs()),
        "settings": {"grace_s": if s.grace.as_secs() > 365 * 86400 { json!("always") } else { json!(s.grace.as_secs()) }, "cleanup": s.cleanup, "budget": s.enforce, "budget_mb": s.budget_mb, "idle_s": s.idle_s},
        "counts": {"windows_ever": s.next, "suspended": s.suspended, "reaped": s.reaped, "mcp_msgs": s.mcp_msgs},
        "reaping": s.reaping, "budget_mb": s.budget_mb, "total_mb": total, "windows": windows, "mcp": mcp, "log": log,
    })
}

/// Write one line. A write that fails may have gone out in part, which would corrupt the next line, so the
/// connection is closed instead. Returns whether the line went out.
fn send(out: &mut UnixStream, v: &Value) -> bool {
    let mut line = v.to_string();
    line.push('\n');
    let ok = out.write_all(line.as_bytes()).is_ok();
    if !ok {
        let _ = out.shutdown(std::net::Shutdown::Both);
    }
    ok
}

fn send_out(out: &Out, v: &Value) -> bool {
    send(&mut out.lock_ok(), v)
}

fn instances(s: &State) -> Value {
    Value::Array(s.insts.values().map(|i| json!({"pid": i.pid, "ui_mb": i.ui_mb, "tabs": i.tabs})).collect())
}

fn broadcast(s: &mut State) {
    let msg = json!({"t": "peers", "instances": instances(s)});
    for i in s.insts.values() {
        send_out(&i.out, &msg);
    }
}

/// One window as the budget sees it.
struct Row<'a> {
    conn: u64,
    ui_mb: u64,
    tabs: &'a [Value],
}

#[derive(Debug, PartialEq)]
struct Pick {
    conn: u64,
    tab: u64,
    total_mb: u64,
    budget_mb: u64,
}

/// A window that did not type for this much longer than the most recent one is in the background, when its terminal
/// does not say whether it has focus. (`input_idle_s` moves in 30 s steps.)
const FRONT_SLACK_S: u64 = 60;

/// Which of these windows are the one you are working in? A terminal that reports focus decides for itself. For one
/// that does not (`focus` null), the window with the most recent input is, and so is any other within `FRONT_SLACK_S`
/// of it. A window that sends nothing (an older build) counts as in front: it is never touched on this account.
fn fronts(wins: &[(u64, &Value)]) -> HashMap<u64, bool> {
    let idle = |w: &Value| w["input_idle_s"].as_u64();
    // A window that does not say how long it has been idle is in front, and is no reference point either: it would
    // otherwise pin `newest` at 0 and make every window you really use look left after a minute of reading.
    let newest = wins.iter().filter_map(|(_, w)| idle(w)).min().unwrap_or(0);
    wins.iter().map(|&(c, w)| (c, w["focus"].as_bool().unwrap_or_else(|| idle(w).is_none_or(|i| i <= newest.saturating_add(FRONT_SLACK_S))))).collect()
}

/// A window's tabs as the picker should see them: in a background window the tab in front is as stoppable as any
/// other (the owner says so with `safe_front`).
fn effective_tabs(tabs: &[Value], front: bool) -> Vec<Value> {
    tabs.iter()
        .map(|t| {
            let mut t = t.clone();
            if !front && t["safe_front"] == true {
                t["safe"] = json!(true);
            }
            t
        })
        .collect()
}

/// Tabs to stop with no memory budget involved, from the idle time each window reports for its tabs and the idle
/// limit `limit` (0: never):
/// - a tab that is not in front, once it has been idle that long;
/// - the tab in front, in a window you are not working in (see `fronts`), once it has been idle that long;
/// - the tab in front of the window you are working in, once you have not typed for five times as long.
///
/// The owner has the last word (`Msg::Suspend` says why), so a report a little old does no harm.
fn idle_asks(insts: &[(u64, &Value, &[Value])], limit: u64) -> Vec<(u64, u64, String)> {
    if limit == 0 {
        return vec![];
    }
    let fr = fronts(&insts.iter().map(|&(c, w, _)| (c, w)).collect::<Vec<_>>());
    let mut asks = vec![];
    for &(c, w, tabs) in insts {
        let away = fr.get(&c) == Some(&false) || w["input_idle_s"].as_u64().unwrap_or(0) >= limit * 5;
        for t in tabs.iter().filter(|t| t["pid"].is_u64() && t["idle_s"].as_u64().unwrap_or(0) >= limit) {
            let idle = t["idle_s"].as_u64().unwrap_or(0);
            let why = if t["safe"] == true {
                format!("the tab was idle {idle} s")
            } else if t["safe_front"] == true && away {
                format!("this window is in the background and the tab was idle {idle} s")
            } else {
                continue;
            };
            asks.push((c, t["id"].as_u64().unwrap_or(0), why));
        }
    }
    asks
}

fn tab_mb(t: &Value) -> u64 {
    t["mb"].as_u64().unwrap_or(0)
}

/// What all windows hold against the budget (0: none): `(held, budget)` in MB, when it is over.
fn over_budget(rows: &[Row], budget_mb: u64) -> Option<(u64, u64)> {
    if budget_mb == 0 {
        return None;
    }
    let total_mb: u64 = rows.iter().map(|r| r.ui_mb + r.tabs.iter().map(tab_mb).sum::<u64>()).sum();
    (total_mb > budget_mb).then_some((total_mb, budget_mb))
}

/// Over budget? Then the stoppable tab (it has a process, and its owner says it is safe to stop) that has been idle
/// longest. Ties go to the bigger one.
fn pick_victim(rows: &[Row], budget_mb: u64) -> Option<Pick> {
    let (total_mb, budget_mb) = over_budget(rows, budget_mb)?;
    let mb = tab_mb;
    rows.iter()
        .flat_map(|r| r.tabs.iter().map(move |t| (r.conn, t)))
        .filter(|(_, t)| t["safe"] == true && t["pid"].is_u64())
        .max_by_key(|(_, t)| (t["idle_s"].as_u64().unwrap_or(0), mb(t)))
        .map(|(conn, t)| Pick { conn, tab: t["id"].as_u64().unwrap_or(0), total_mb, budget_mb })
}

/// Serve until the last instance has left and `grace` has passed. Returns at once when another supervisor is
/// already serving this directory.
pub fn run(paths: &Paths, cfg: &Config) -> io::Result<()> {
    paths.prepare()?;
    let sock = paths.sock();
    if UnixStream::connect(&sock).is_ok() {
        return Ok(());
    }
    let _ = fs::remove_file(&sock); // a stale file from a supervisor that did not get to clean up
    let listener = UnixListener::bind(&sock)?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    paths.log(&format!("start pid={}", std::process::id()));
    let state = Arc::new(Mutex::new(State::new(cfg)));
    let mut pace = Pace::new();
    let quit = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false); // macOS: an accepted socket inherits the listener's non-blocking mode
                let (state, paths, cfg) = (state.clone(), paths.clone(), *cfg);
                thread::spawn(move || serve(stream, state, &paths, &cfg));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(40)),
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
        if let Some(q) = step(&state, paths, cfg, &mut pace) {
            break q;
        }
    };
    let _ = fs::remove_file(&sock);
    paths.log(if quit { "exit: asked to" } else { "exit: no instance left" });
    Ok(())
}

/// What the supervisor does between connections: measure, apply the idle sweep and the memory budget, and decide
/// whether it is time to leave (`Some(true)`: it was asked to; `Some(false)`: no window is left).
fn step(state: &Arc<Mutex<State>>, paths: &Paths, cfg: &Config, pace: &mut Pace) -> Option<bool> {
    measure(state, cfg, pace);
    let mut s = state.lock().unwrap();
    if pace.budget.elapsed() >= cfg.budget_every && s.enforce {
        pace.budget = Instant::now();
        let wins: Vec<(u64, &Value)> = s.insts.iter().map(|(&c, i)| (c, &i.win)).collect();
        let fr = fronts(&wins);
        let eff: Vec<(u64, Vec<Value>)> = s.insts.iter().map(|(&c, i)| (c, effective_tabs(&i.tabs, fr.get(&c).copied().unwrap_or(true)))).collect();
        let (pick, over) = {
            let rows: Vec<Row> = s.insts.iter().map(|(&conn, i)| Row { conn, ui_mb: i.ui_mb, tabs: &eff.iter().find(|e| e.0 == conn).unwrap().1 }).collect();
            (pick_victim(&rows, s.budget_mb), over_budget(&rows, s.budget_mb))
        };
        let mut asks = idle_asks(&s.insts.iter().map(|(&c, i)| (c, &i.win, &i.tabs[..])).collect::<Vec<_>>(), s.idle_s);
        // The budget's one tab per check: when the sweep is already stopping tabs, the next sample shows what that freed.
        if asks.is_empty() {
            match (pick, over) {
                (Some(p), _) => asks.push((p.conn, p.tab, format!("all claude++ windows hold {} MB, budget {} MB", p.total_mb, p.budget_mb))),
                (None, Some((held, budget))) if pace.noticed.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) => {
                    pace.noticed = Some(Instant::now());
                    let text = json!({"t": "notice", "text": format!("over the memory budget ({held} MB held, {budget} MB allowed) and no tab is safe to stop")});
                    s.insts.values().for_each(|i| {
                        send_out(&i.out, &text);
                    });
                }
                _ => {}
            }
        }
        let (again, idle_s) = (cfg.budget_every * 5, s.idle_s);
        s.asked.retain(|_, at| at.elapsed() < again.max(Duration::from_secs(300)));
        asks.retain(|(c, t, _)| s.asked.get(&(*c, *t)).is_none_or(|at| at.elapsed() >= again));
        for (conn, tab, why) in asks {
            let Some(i) = s.insts.get(&conn) else { continue };
            let front = i.tabs.iter().any(|t| t["id"].as_u64() == Some(tab) && t["safe"] != true && t["safe_front"] == true);
            paths.log(&format!("asking instance {} to stop tab {}{}: {why}", i.pid, tab, if front { " (in front)" } else { "" }));
            send_out(&i.out, &json!({"t": "suspend", "tab": tab, "why": why, "front": front, "limit_s": idle_s}));
            s.suspended += 1;
            s.asked.insert((conn, tab), Instant::now());
        }
    }
    if s.insts.is_empty() && s.reaping == 0 {
        if pace.empty_since.get_or_insert_with(Instant::now).elapsed() >= s.grace {
            return Some(false);
        }
    } else {
        pace.empty_since = None;
    }
    (s.quit && s.reaping == 0).then_some(true)
}

/// What the windows' processes hold: each tab's claude process with its children, and each window itself. Measured
/// here, for every window at once, so no window runs `ps` and `footprint` of its own. `ps` and `footprint` take
/// 20–90 ms, so not under the lock. Windows hear about it in the next `peers` message.
fn measure(state: &Arc<Mutex<State>>, cfg: &Config, pace: &mut Pace) {
    let every = if state.lock().unwrap().budget_mb > 0 { cfg.budget_every } else { cfg.budget_every * 3 };
    if !cfg.measure || pace.measure.elapsed() < every {
        return;
    }
    let (tabs, wins): (Vec<u32>, Vec<u32>) = {
        let s = state.lock().unwrap();
        (s.insts.values().flat_map(|i| i.tabs.iter().filter_map(|t| t["pid"].as_u64())).map(|p| p as u32).collect(), s.insts.values().map(|i| i.pid).collect())
    };
    if wins.is_empty() {
        return; // the first window gets its numbers at once, not after a whole interval
    }
    pace.measure = Instant::now();
    let mem = if tabs.is_empty() { vec![] } else { crate::memory::tree_mem_kb(&tabs) };
    let ui = crate::memory::held_kb(&wins).0;
    let mut s = state.lock().unwrap();
    let before = instances(&s);
    for i in s.insts.values_mut() {
        i.ui_mb = ui.iter().find(|r| r.0 == i.pid).map_or(0, |r| r.1 / 1024);
        for t in &mut i.tabs {
            if let Some(kb) = t["pid"].as_u64().and_then(|p| mem.iter().find(|r| r.0 == p as u32)).map(|r| r.1) {
                t["mb"] = json!(kb / 1024);
            }
        }
    }
    if instances(&s) != before {
        broadcast(&mut s);
    }
}

fn serve(stream: UnixStream, state: Arc<Mutex<State>>, paths: &Paths, cfg: &Config) {
    let Ok(read) = stream.try_clone() else { return };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    let out: Out = Arc::new(Mutex::new(stream));
    let mut id = None;
    // Shared MCP servers this window uses; they stop when the last window using one has gone.
    let mut servers: HashMap<String, Arc<share::Server>> = HashMap::new();
    for line in BufReader::new(read).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match (v["t"].as_str(), id) {
            (Some("hello"), None) => {
                let pid = v["pid"].as_u64().unwrap_or(0) as u32;
                if v["v"].as_u64() != Some(VERSION) {
                    send_out(&out, &json!({"t": "refuse", "why": format!("protocol {VERSION} only")}));
                    return;
                }
                let mut s = state.lock().unwrap();
                s.next += 1;
                id = Some(s.next);
                let key = s.next;
                s.insts.insert(key, Inst { pid, stamp: stamp(pid), tabs: vec![], owned: vec![], bye: false, ui_mb: 0, win: Value::Null, out: out.clone() });
                send_out(&out, &json!({"t": "welcome", "v": VERSION, "instances": instances(&s), "features": ["mcp", "manage", "measure"], "pid": std::process::id(), "exe_mtime": s.exe_mtime}));
                broadcast(&mut s);
                paths.log(&format!("instance {pid} connected"));
            }
            (Some("tabs"), Some(id)) => {
                let mut tabs: Vec<Value> = v["tabs"].as_array().cloned().unwrap_or_default();
                let owned: Vec<u32> = v["owned"].as_array().into_iter().flatten().filter_map(|p| p.as_u64()).map(|p| p as u32).collect();
                let mut s = state.lock().unwrap();
                let Some(i) = s.insts.get_mut(&id) else { return };
                // A pid is stamped when first reported, while its owner is alive and has just started it.
                let me = std::process::id();
                let keep: Vec<(u32, u64)> = owned.into_iter().filter(|&p| p > 1 && p != me && p != i.pid).filter_map(|p| i.owned.iter().find(|o| o.0 == p).copied().or_else(|| stamp(p).map(|st| (p, st)))).collect();
                // What the supervisor measured stays until its next measurement; the window does not say.
                for t in &mut tabs {
                    if t["mb"].is_null()
                        && let Some(mb) = i.tabs.iter().find(|o| o["id"] == t["id"] && o["pid"] == t["pid"]).map(|o| o["mb"].clone())
                    {
                        t["mb"] = mb;
                    }
                }
                let changed = i.tabs != tabs;
                i.win = v["win"].clone();
                (i.tabs, i.owned) = (tabs, keep);
                if changed {
                    broadcast(&mut s);
                }
            }
            (Some("status"), Some(_)) => {
                let mcp = mcp_rows();
                let st = status(&state.lock().unwrap(), paths, mcp);
                send_out(&out, &st);
            }
            (Some("set"), Some(_)) => {
                let (key, value) = (v["key"].as_str().unwrap_or(""), v["value"].as_str().unwrap_or(""));
                let mut s = state.lock().unwrap();
                match key {
                    "grace_s" => s.grace = parse_grace(value).unwrap_or(s.grace),
                    "cleanup" => s.cleanup = value != "off",
                    "budget" => s.enforce = value != "paused",
                    "budget_mb" => s.budget_mb = value.parse().unwrap_or(s.budget_mb),
                    "idle_s" => s.idle_s = value.parse().unwrap_or(s.idle_s),
                    _ => {}
                }
                paths.log(&format!("{key} = {value}"));
                drop(s);
                let mcp = mcp_rows();
                let st = status(&state.lock().unwrap(), paths, mcp);
                send_out(&out, &st);
            }
            (Some("mcp_restart"), Some(_)) => {
                let key = v["key"].as_str().unwrap_or("");
                paths.log(&format!("restarting a shared MCP server: {}", if share::restart(key) { "done" } else { "it was gone" }));
            }
            (Some("shutdown"), Some(_)) => {
                paths.log("asked to shut down");
                state.lock().unwrap().quit = true;
            }
            (Some("bye"), Some(id)) => {
                if let Some(i) = state.lock().unwrap().insts.get_mut(&id) {
                    i.bye = true;
                }
            }
            (Some("mcp"), Some(conn)) => {
                let (Some(rid), Some(def)) = (v["id"].as_u64(), Def::from_json(&v["def"])) else { continue };
                let msg = v["msg"].clone();
                state.lock().unwrap().mcp_msgs += 1;
                // Ids are per window; the conn id keeps two windows' client numbers apart.
                let client = (conn << 32) | (v["client"].as_u64().unwrap_or(0) & 0xffff_ffff);
                let reply: Reply = {
                    let out = out.clone();
                    Box::new(move |resp| {
                        send_out(&out, &json!({"t": "mcp_reply", "id": rid, "resp": resp}));
                    })
                };
                // Not under the state lock: starting the process takes a moment.
                let server = match servers.get(&def.key()) {
                    Some(s) => Ok(s.clone()),
                    None => share::acquire(&def).inspect(|s| {
                        servers.insert(def.key(), s.clone());
                    }),
                };
                match server {
                    Ok(s) => s.route(client, msg, reply),
                    Err(e) => reply(share::error(msg.get("id").unwrap_or(&Value::Null), &e)),
                }
            }
            _ => {}
        }
    }
    drop(servers);
    let Some(id) = id else { return };
    let mut s = state.lock().unwrap();
    let Some(gone) = s.insts.remove(&id) else { return };
    broadcast(&mut s);
    paths.log(&format!("instance {} left{}", gone.pid, if gone.bye { "" } else { " without saying goodbye" }));
    if !gone.bye && !gone.owned.is_empty() && s.cleanup {
        s.reaping += 1;
        let (paths, kill_after, reap_after) = (paths.clone(), cfg.kill_after, cfg.reap_after);
        let state = state.clone();
        drop(s);
        thread::spawn(move || {
            thread::sleep(reap_after);
            let stopped = reap(&gone, &paths, kill_after);
            let mut s = state.lock().unwrap();
            s.reaping -= 1;
            s.reaped += stopped;
        });
    }
}

/// Stop what a vanished instance left running. Never when the instance itself still runs (it may only have lost
/// its connection), and never a pid whose start time changed.
fn reap(gone: &Inst, paths: &Paths, kill_after: Duration) -> u64 {
    let mut stopped = 0;
    let Some(st) = gone.stamp else {
        paths.log(&format!("instance {}: start time unknown, leaving its processes", gone.pid));
        return 0;
    };
    if same(gone.pid, st) {
        paths.log(&format!("instance {} is still running, leaving its processes", gone.pid));
        return 0;
    }
    for &(pid, st) in &gone.owned {
        if !same(pid, st) {
            continue;
        }
        paths.log(&format!("stopping {pid}, left by instance {}", gone.pid));
        stopped += 1;
        // SAFETY: plain signals to a pid whose start time was just checked.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let t = Instant::now();
        while t.elapsed() < kill_after && same(pid, st) {
            thread::sleep(Duration::from_millis(50));
        }
        if same(pid, st) {
            paths.log(&format!("{pid} ignored SIGTERM, killing it"));
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
    stopped
}

pub struct TabInfo {
    pub id: u64,
    pub pid: Option<u32>,
    pub busy: bool,
    pub mb: u64,
    pub cwd: String,
}

pub struct Peer {
    pub pid: u32,
    /// What the window itself holds, as the supervisor measured it.
    pub ui_mb: u64,
    pub tabs: Vec<TabInfo>,
}

/// What the supervisor sends an instance.
pub enum Msg {
    /// Every instance, this one included, and what their tabs are doing. Empty once the connection is gone.
    Peers(Vec<Peer>),
    /// The memory budget is exceeded and this tab is the one to stop, if its owner still finds it safe to.
    /// `front`: the tab is the one in front in a window the supervisor found to be in the background.
    Suspend { tab: u64, why: String, front: bool, limit_s: u64 },
    /// Something the supervisor wants said in the window (a line for the status flash).
    Notice(String),
    /// Its state, answering `ask_status` or `set` (see `/supervisor`).
    Status(Value),
}

fn parse_peers(v: &Value) -> Vec<Peer> {
    v["instances"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| Peer {
            pid: i["pid"].as_u64().unwrap_or(0) as u32,
            ui_mb: i["ui_mb"].as_u64().unwrap_or(0),
            tabs: i["tabs"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| TabInfo { id: t["id"].as_u64().unwrap_or(0), pid: t["pid"].as_u64().map(|p| p as u32), busy: t["busy"].as_bool().unwrap_or(false), mb: t["mb"].as_u64().unwrap_or(0), cwd: t["cwd"].as_str().unwrap_or("").into() })
                .collect(),
        })
        .collect()
}

/// This instance's side of the connection. Every call is non-blocking in practice: a stalled supervisor costs
/// a dropped message, never a frozen screen.
pub struct Client {
    out: Mutex<UnixStream>,
    last: Mutex<(Value, Instant)>,
    /// What this window says about itself, sent with every `tabs` message (see `Inst::win`).
    win: Mutex<Value>,
    alive: Arc<AtomicBool>,
    /// The supervisor can run shared MCP servers for us.
    mcp: bool,
    /// It answers `status` and the other management messages (an older build does not).
    manage: bool,
    /// It measures what the windows hold and applies the idle sweep to every tab. An older one leaves both to the window.
    measures: bool,
    /// Its process, from its welcome or else from the socket.
    pid: Option<u32>,
    next_mcp: AtomicU64,
    /// Inside this process (`Client::private`), not the shared one.
    private: bool,
    /// The supervisor runs an older build than this window and nobody else is attached to it: safe to replace.
    stale_alone: bool,
    /// Messages handed to the supervisor that have no answer yet: the reply to call, and the JSON-RPC id.
    pending: Pending,
}

type Pending = Arc<Mutex<HashMap<u64, (Reply, Value)>>>;

impl Client {
    /// Connect and say hello. `wrap` turns each message the supervisor sends into one of your events.
    pub fn connect<E: Send + 'static>(paths: &Paths, pid: u32, tx: Sender<E>, wrap: impl Fn(Msg) -> E + Send + 'static) -> Option<Client> {
        Client::attach(UnixStream::connect(paths.sock()).ok()?, false, pid, tx, wrap)
    }

    /// A supervisor of this process's own, for a window that cannot use the shared one (it was switched off or stopped
    /// on purpose, or would not start). The same code and the same messages, over a socket pair, so the memory budget
    /// and the idle sweep work as they do everywhere and the window has no second implementation of either. It serves
    /// this window only and goes with it.
    pub fn private<E: Send + 'static>(pid: u32, tx: Sender<E>, wrap: impl Fn(Msg) -> E + Send + 'static) -> Option<Client> {
        let (mine, theirs) = UnixStream::pair().ok()?;
        let cfg = Config { grace: Duration::from_secs(5), cleanup: false, ..Config::user() };
        let paths = Paths { dir: PathBuf::from("/nonexistent/claudepp-private") }; // nothing is logged
        let state = Arc::new(Mutex::new(State::new(&cfg)));
        {
            let (state, paths) = (state.clone(), paths.clone());
            thread::spawn(move || serve(theirs, state, &paths, &cfg));
        }
        thread::spawn(move || {
            let mut pace = Pace::new();
            // Only "no window left" ends it: `/supervisor` cannot stop what this window depends on.
            while step(&state, &paths, &cfg, &mut pace) != Some(false) {
                thread::sleep(Duration::from_millis(40));
            }
        });
        Client::attach(mine, true, pid, tx, wrap)
    }

    /// Whether this is the window's own supervisor and not the shared one.
    pub fn is_private(&self) -> bool {
        self.private
    }

    fn attach<E: Send + 'static>(stream: UnixStream, private: bool, pid: u32, tx: Sender<E>, wrap: impl Fn(Msg) -> E + Send + 'static) -> Option<Client> {
        stream.set_write_timeout(Some(Duration::from_millis(300))).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
        let mut out = stream.try_clone().ok()?;
        send(&mut out, &json!({"t": "hello", "v": VERSION, "pid": pid}));
        let mut reader = BufReader::new(stream);
        let mut first = String::new();
        reader.read_line(&mut first).ok()?;
        let welcome: Value = serde_json::from_str(&first).ok()?;
        if welcome["t"] != "welcome" {
            return None;
        }
        reader.get_ref().set_read_timeout(None).ok()?;
        let has = |name: &str| welcome["features"].as_array().is_some_and(|f| f.iter().any(|x| x == name));
        let (mcp, manage, measures) = (has("mcp"), has("manage"), has("measure"));
        let pid = welcome["pid"].as_u64().map(|p| p as u32).or_else(|| peer_pid(reader.get_ref()));
        let alive = Arc::new(AtomicBool::new(true));
        let flag = alive.clone();
        let pending: Pending = Default::default();
        let waiting = pending.clone();
        // A welcome without `exe_mtime` comes from a build older than the field.
        let mine = exe_mtime();
        let stale_alone = mine != 0 && welcome["exe_mtime"].as_u64().unwrap_or(1) != mine && welcome["instances"].as_array().is_some_and(|a| a.len() <= 1);
        let _ = tx.send(wrap(Msg::Peers(parse_peers(&welcome))));
        thread::spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let msg = match v["t"].as_str() {
                    Some("mcp_reply") => {
                        let done = v["id"].as_u64().and_then(|id| waiting.lock_ok().remove(&id));
                        if let Some((reply, _)) = done {
                            reply(v["resp"].clone());
                        }
                        continue;
                    }
                    Some("peers") => Msg::Peers(parse_peers(&v)),
                    Some("status") => Msg::Status(v),
                    Some("notice") => Msg::Notice(v["text"].as_str().unwrap_or("").into()),
                    Some("suspend") => Msg::Suspend { tab: v["tab"].as_u64().unwrap_or(0), why: v["why"].as_str().unwrap_or("").into(), front: v["front"].as_bool().unwrap_or(false), limit_s: v["limit_s"].as_u64().unwrap_or(0) },
                    _ => continue,
                };
                if tx.send(wrap(msg)).is_err() {
                    break;
                }
            }
            // Under the lock, so a message sent now either sees the connection gone or is answered below.
            let lost: Vec<_> = {
                let mut w = waiting.lock_ok();
                flag.store(false, Ordering::Relaxed);
                w.drain().map(|(_, p)| p).collect()
            };
            for (reply, id) in lost {
                reply(share::error(&id, "the supervisor went away"));
            }
            let _ = tx.send(wrap(Msg::Peers(vec![])));
        });
        Some(Client { out: Mutex::new(out), last: Mutex::new((Value::Null, Instant::now())), win: Mutex::new(Value::Null), alive, mcp, manage, measures, pid, next_mcp: AtomicU64::new(0), private, stale_alone, pending })
    }

    /// Connect, starting the supervisor first when none is running. Takes up to ~2 s: call it off the UI thread.
    pub fn connect_or_start<E: Send + 'static>(paths: &Paths, pid: u32, tx: Sender<E>, wrap: impl Fn(Msg) -> E + Send + Clone + 'static) -> Option<Client> {
        if let Some(c) = Client::connect(paths, pid, tx.clone(), wrap.clone()) {
            if !c.stale_alone {
                return Some(c);
            }
            // Left over from before this build was installed, and serving no one else: replace it, so its fixes apply.
            c.shutdown();
            drop(c);
            for _ in 0..30 {
                if UnixStream::connect(paths.sock()).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        if paths.stopped_marker().exists() {
            return None; // stopped on purpose, with `/supervisor`
        }
        start_supervisor()?;
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(100));
            if let Some(c) = Client::connect(paths, pid, tx.clone(), wrap.clone()) {
                return Some(c);
            }
        }
        None
    }

    /// Whether it does the measuring and the idle sweep for this window: a window leaves both to it.
    pub fn measures(&self) -> bool {
        self.measures
    }

    /// Whether it can be managed with messages; an older build can only be signalled.
    pub fn manages(&self) -> bool {
        self.manage
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Ask for its state: it arrives as `Msg::Status`.
    pub fn ask_status(&self) {
        send(&mut self.out.lock_ok(), &json!({"t": "status"}));
    }

    /// Change one of its settings (`grace_s`, `cleanup`, `budget`); the new state arrives as `Msg::Status`.
    pub fn set(&self, key: &str, value: &str) {
        send(&mut self.out.lock_ok(), &json!({"t": "set", "key": key, "value": value}));
    }

    pub fn restart_mcp(&self, key: &str) {
        send(&mut self.out.lock_ok(), &json!({"t": "mcp_restart", "key": key}));
    }

    /// Ask it to leave; windows start a new one within ~10 s unless it was stopped on purpose.
    pub fn shutdown(&self) {
        send(&mut self.out.lock_ok(), &json!({"t": "shutdown"}));
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// What this instance's tabs are doing, the pids of the processes it started, the memory budget it asked for
    /// (0: none). The supervisor measures what they hold. Sent when it changed, and
    /// every 10 s regardless so the supervisor can tell a quiet instance from a gone one.
    pub fn tabs(&self, tabs: Vec<Value>, owned: Vec<u32>) {
        let win = self.win.lock_ok().clone();
        let msg = json!({"t": "tabs", "tabs": tabs, "owned": owned, "win": win});
        let mut last = self.last.lock().unwrap();
        if last.0 == msg && last.1.elapsed() < Duration::from_secs(10) {
            return;
        }
        *last = (msg.clone(), Instant::now());
        send(&mut self.out.lock().unwrap(), &msg);
    }

    /// Say what this window is doing as a whole: `focus` (its terminal has focus; `None`: the terminal does not say),
    /// `input_idle_s` (since the last key, in 30 s steps). Goes out with the next `tabs`.
    pub fn window(&self, focus: Option<bool>, input_idle_s: u64) {
        *self.win.lock_ok() = json!({"focus": focus, "input_idle_s": input_idle_s});
    }

    /// Hand one message of a shared MCP server to the supervisor; `reply` gets the answer (or an error if the
    /// connection is lost first). `Err` gives the reply back when the supervisor cannot take it, so the caller
    /// runs the server itself.
    pub fn mcp(&self, def: &Def, client: u64, msg: Value, reply: Reply) -> Result<(), Reply> {
        let mut pending = self.pending.lock_ok();
        if !self.mcp || !self.alive() {
            return Err(reply);
        }
        let id = self.next_mcp.fetch_add(1, Ordering::Relaxed) + 1;
        let line = json!({"t": "mcp", "id": id, "client": client, "def": def.to_json(), "msg": msg});
        pending.insert(id, (reply, msg.get("id").cloned().unwrap_or(Value::Null)));
        if send(&mut self.out.lock_ok(), &line) {
            return Ok(());
        }
        Err(pending.remove(&id).map(|p| p.0).expect("just inserted"))
    }

    /// For `share::set_hub`. Holds the client weakly: dropping the client still closes the connection.
    pub fn hub(self: &Arc<Self>) -> Arc<share::HubFn> {
        let c = Arc::downgrade(self);
        Arc::new(move |def, client, msg, reply| match c.upgrade() {
            Some(c) => c.mcp(def, client, msg, reply),
            None => Err(reply),
        })
    }

    /// A clean exit: the supervisor will not touch anything this instance leaves.
    pub fn bye(&self) {
        let mut out = self.out.lock().unwrap();
        send(&mut out, &json!({"t": "bye"}));
        let _ = out.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Client {
    /// Without this the reader thread would keep the connection open after the client is gone.
    fn drop(&mut self) {
        let _ = self.out.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

/// Stopped on purpose: windows leave it stopped until `allow_start`.
pub fn stop_on_purpose(paths: &Paths) -> io::Result<()> {
    paths.prepare()?;
    fs::write(paths.stopped_marker(), "")
}

pub fn allow_start(paths: &Paths) {
    let _ = fs::remove_file(paths.stopped_marker());
}

/// The process on the other end of a connected socket.
#[cfg(target_os = "macos")]
fn peer_pid(s: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let (mut pid, mut len) = (0 as libc::pid_t, std::mem::size_of::<libc::pid_t>() as libc::socklen_t);
    // SAFETY: `pid` and `len` are valid for the size given. SOL_LOCAL = 0, LOCAL_PEERPID = 2.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), 0, 2, &mut pid as *mut _ as *mut libc::c_void, &mut len) };
    (r == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(not(target_os = "macos"))]
fn peer_pid(s: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the size given.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
    (r == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// An older supervisor does not understand `shutdown`: stop it with a signal. Only a process that really is a
/// `claude++ supervisor` (its command line says so): the pid came from a socket in your private directory,
/// but a signal is not something to send on trust.
pub fn terminate_old(pid: u32) -> bool {
    let out = std::process::Command::new("ps").args(["-o", "command=", "-p", &pid.to_string()]).output().ok();
    let ours = out.is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim_end().ends_with(" supervisor"));
    // SAFETY: a plain SIGTERM to a process just checked to be a claude++ supervisor.
    ours && unsafe { libc::kill(pid as i32, libc::SIGTERM) } == 0
}

/// `claude++ supervisor`, detached from this terminal so it outlives the window that started it.
fn start_supervisor() -> Option<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(std::env::current_exe().ok()?);
    cmd.arg("supervisor").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe; it makes the child its own session leader.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    // Reaped by a thread that waits for it: a window that lost the race to start it (another supervisor was already
    // serving, so this one exited at once) would otherwise keep a zombie for as long as the window lives.
    cmd.spawn().ok().map(|mut child| {
        thread::spawn(move || {
            let _ = child.wait();
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;

    fn paths(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!("cpp-sup-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Paths { dir }
    }

    fn fast() -> Config {
        Config { grace: Duration::from_millis(300), reap_after: Duration::from_millis(200), kill_after: Duration::from_millis(500), budget_every: Duration::from_millis(200), cleanup: true, enforce: true, measure: false, budget_mb: 0, idle_s: 0 }
    }

    fn serve_in_background(p: &Paths, cfg: Config) -> mpsc::Receiver<()> {
        let (p, (tx, rx)) = (p.clone(), mpsc::channel());
        thread::spawn(move || {
            run(&p, &cfg).unwrap();
            let _ = tx.send(());
        });
        rx
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let t = Instant::now();
        while !ok() {
            assert!(t.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn client(p: &Paths, pid: u32) -> (Client, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        (Client::connect(p, pid, tx, |m| m).expect("connect"), rx)
    }

    fn sleeper() -> Child {
        Command::new("sleep").arg("60").stdin(Stdio::null()).spawn().unwrap()
    }

    #[test]
    fn instances_see_each_others_tabs_and_the_supervisor_leaves_after_the_last_one() {
        let p = paths("peers");
        let done = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());
        let (a, _ra) = client(&p, std::process::id());
        let (b, rb) = client(&p, std::process::id());
        a.tabs(vec![json!({"id": 1, "pid": 4242, "busy": true, "mb": 190, "cwd": "/x"})], vec![]);
        let seen = loop {
            if let Msg::Peers(peers) = rb.recv_timeout(Duration::from_secs(5)).expect("a peers message")
                && peers.iter().any(|i| !i.tabs.is_empty())
            {
                break peers;
            }
        };
        let tab = &seen.iter().find(|i| !i.tabs.is_empty()).unwrap().tabs[0];
        assert_eq!((tab.id, tab.pid, tab.busy, tab.mb, tab.cwd.as_str()), (1, Some(4242), true, 190, "/x"));
        a.bye();
        assert!(done.recv_timeout(Duration::from_millis(500)).is_err(), "one instance is still connected");
        b.bye();
        done.recv_timeout(Duration::from_secs(5)).expect("it leaves once nobody is left");
        assert!(!p.sock().exists(), "the socket goes with it");
    }

    #[test]
    fn a_second_supervisor_defers_and_a_stale_socket_is_replaced() {
        let p = paths("twice");
        let done = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());
        run(&p, &fast()).unwrap(); // returns at once: one is serving
        done.recv_timeout(Duration::from_secs(5)).expect("the first one still ends by itself");
        drop(UnixListener::bind(p.sock()).unwrap()); // a file nobody listens on
        assert!(p.sock().exists());
        let done = serve_in_background(&p, fast());
        wait_for("a live socket", || UnixStream::connect(p.sock()).is_ok());
        done.recv_timeout(Duration::from_secs(5)).expect("it served, then ended");
    }

    #[test]
    fn a_supervisor_from_before_it_measured_is_known_by_its_welcome() {
        let p = paths("oldwelcome");
        p.prepare().unwrap();
        let listener = UnixListener::bind(p.sock()).unwrap();
        let old = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut hello = String::new();
            BufReader::new(s.try_clone().unwrap()).read_line(&mut hello).unwrap();
            writeln!(s, r#"{{"t":"welcome","v":1,"instances":[],"features":["mcp","manage"],"pid":1}}"#).unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let (c, _rx) = client(&p, std::process::id());
        assert!(c.manages() && !c.measures(), "it can be managed but leaves the measuring to the window");
        old.join().unwrap();
        let q = paths("newwelcome");
        let _ = serve_in_background(&q, fast());
        wait_for("the socket", || q.sock().exists());
        assert!(client(&q, std::process::id()).0.measures());
    }

    #[test]
    fn a_protocol_it_does_not_speak_is_refused() {
        let p = paths("refuse");
        let _ = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());
        let mut s = UnixStream::connect(p.sock()).unwrap();
        send(&mut s, &json!({"t": "hello", "v": 99, "pid": 1}));
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        assert!(line.contains("refuse"), "{line}");
    }

    /// An instance that dies without a goodbye: `fake` stands in for it (a real, short-lived process).
    fn vanish(p: &Paths, fake: &mut Child, owned: &Child, say_bye: bool) {
        let (c, _r) = client(p, fake.id());
        c.tabs(vec![json!({"id": 1})], vec![owned.id()]);
        // Let the supervisor read the report before the "instance" goes away.
        thread::sleep(Duration::from_millis(300));
        fake.kill().unwrap();
        fake.wait().unwrap();
        if say_bye {
            c.bye();
        }
        drop(c);
    }

    #[test]
    fn what_a_vanished_instance_started_is_stopped_but_not_after_a_goodbye() {
        let p = paths("reap");
        let done = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());

        let (mut fake, mut backend) = (sleeper(), sleeper());
        vanish(&p, &mut fake, &backend, false);
        wait_for("the orphan to be stopped", || backend.try_wait().unwrap().is_some());

        let (mut fake, mut kept) = (sleeper(), sleeper());
        vanish(&p, &mut fake, &kept, true);
        thread::sleep(Duration::from_millis(1200));
        assert!(kept.try_wait().unwrap().is_none(), "a clean exit leaves its processes alone");
        kept.kill().unwrap();
        done.recv_timeout(Duration::from_secs(8)).expect("it ends once nothing is left to look at");
    }

    #[test]
    fn a_live_instance_that_only_lost_its_connection_keeps_its_processes() {
        let p = paths("alive");
        let _ = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());
        let (mut fake, mut backend) = (sleeper(), sleeper());
        let (c, _r) = client(&p, fake.id());
        c.tabs(vec![json!({"id": 1})], vec![backend.id()]);
        thread::sleep(Duration::from_millis(300));
        drop(c); // the connection ends, the "instance" (fake) keeps running
        thread::sleep(Duration::from_millis(1200));
        assert!(backend.try_wait().unwrap().is_none());
        fake.kill().unwrap();
        backend.kill().unwrap();
    }

    #[test]
    fn a_process_is_identified_by_pid_and_start_time() {
        let mut c = sleeper();
        let st = stamp(c.id()).expect("a running process has a start time");
        assert!(same(c.id(), st) && !same(c.id(), st + 1));
        c.kill().unwrap();
        c.wait().unwrap();
        assert_eq!(stamp(c.id()), None);
    }

    fn tab(id: u64, mb: u64, idle_s: u64, safe: bool) -> Value {
        json!({"id": id, "pid": 1000 + id, "mb": mb, "idle_s": idle_s, "safe": safe})
    }

    #[test]
    fn over_the_budget_the_longest_idle_stoppable_tab_of_any_window_goes() {
        let (a, b, c) = (vec![tab(1, 100, 600, true), tab(2, 100, 30, true)], vec![tab(3, 100, 900, true)], vec![tab(4, 100, 5000, false)]);
        let rows = [Row { conn: 1, ui_mb: 10, tabs: &a }, Row { conn: 2, ui_mb: 10, tabs: &b }, Row { conn: 3, ui_mb: 10, tabs: &c }];
        // 430 MB in all. Tab 4 is idle longest of all but its owner says it cannot be stopped.
        assert_eq!(pick_victim(&rows, 400), Some(Pick { conn: 2, tab: 3, total_mb: 430, budget_mb: 400 }), "tab 3: idle longer than tabs 1 and 2, and stoppable unlike tab 4");
        assert_eq!(pick_victim(&rows, 431), None, "430 MB is within a 431 MB budget");
        assert_eq!(pick_victim(&rows, 0), None, "no budget, nothing to enforce");
        assert_eq!(over_budget(&rows, 400), Some((430, 400)));
    }

    #[test]
    fn tabs_that_cannot_be_stopped_are_never_picked() {
        let tabs = vec![tab(1, 300, 9000, false), json!({"id": 2, "pid": null, "mb": 0, "idle_s": 9000, "safe": true}), tab(3, 300, 10, true)];
        let rows = [Row { conn: 1, ui_mb: 0, tabs: &tabs }];
        assert_eq!(pick_victim(&rows, 100).map(|p| p.tab), Some(3), "not tab 1 (owner says unsafe) and not tab 2 (no process)");
        let none = vec![tab(1, 300, 9000, false)];
        assert_eq!(pick_victim(&[Row { conn: 1, ui_mb: 0, tabs: &none }], 100), None);
        assert_eq!(pick_victim(&[], 100), None);
    }

    #[test]
    fn the_window_that_owns_the_tab_is_asked_and_nobody_is_asked_once_it_is_under_budget() {
        let p = paths("budget");
        let _ = serve_in_background(&p, Config { budget_mb: 200, ..fast() });
        wait_for("the socket", || p.sock().exists());
        let (a, ra) = client(&p, std::process::id());
        let (b, rb) = client(&p, std::process::id());
        // A holds 150 MB in two tabs and B 120 MB in one, against a 200 MB budget: 270 MB in all.
        a.tabs(vec![tab(1, 80, 600, true), tab(2, 70, 30, true)], vec![]);
        b.tabs(vec![tab(9, 120, 99999, true)], vec![]);
        let ask = loop {
            if let Msg::Suspend { tab, why, .. } = rb.recv_timeout(Duration::from_secs(5)).expect("a request to B") {
                break (tab, why);
            }
        };
        assert_eq!(ask.0, 9, "the longest-idle tab of all windows, which is B's");
        assert!(ask.1.contains("270 MB") && ask.1.contains("200 MB"), "{}", ask.1);
        assert!(!std::iter::from_fn(|| ra.try_recv().ok()).any(|m| matches!(m, Msg::Suspend { .. })), "A is not asked while B's tab is the one to go");
        // B stops tab 9 and reports: 150 MB in all, under budget.
        b.tabs(vec![json!({"id": 9, "pid": null, "mb": 0, "idle_s": 99999, "safe": false})], vec![]);
        thread::sleep(Duration::from_millis(100));
        while ra.try_recv().is_ok() || rb.try_recv().is_ok() {}
        thread::sleep(Duration::from_millis(700));
        assert!(!std::iter::from_fn(|| ra.try_recv().ok().or(rb.try_recv().ok())).any(|m| matches!(m, Msg::Suspend { .. })), "under budget: no more requests");
    }

    fn mcp_ask(c: &Client, def: &Def, client: u64, msg: Value) -> Value {
        let (tx, rx) = mpsc::channel();
        c.mcp(def, client, msg, Box::new(move |v| tx.send(v).unwrap())).map_err(drop).expect("the supervisor takes it");
        rx.recv_timeout(Duration::from_secs(5)).expect("a reply")
    }

    #[test]
    fn windows_share_one_mcp_process_in_the_supervisor_and_it_ends_with_the_last_of_them() {
        let _own = share::tests::HUB_TEST.lock_ok();
        let p = paths("mcp");
        let done = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());
        let (def, log) = share::tests::server("sup");
        let (a, _ra) = client(&p, std::process::id());
        let (b, _rb) = client(&p, std::process::id());
        let init = |id: u64| json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}});
        let call = |id: u64| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {}});
        // Both windows use client number 1 and request id 7: the answers must still reach the right one.
        assert_eq!(mcp_ask(&a, &def, 1, init(7))["id"], 7);
        assert_eq!(mcp_ask(&b, &def, 1, init(7))["id"], 7);
        let (ra, rb) = (mcp_ask(&a, &def, 1, call(8)), mcp_ask(&b, &def, 1, call(9)));
        assert_eq!((&ra["id"], &rb["id"]), (&json!(8), &json!(9)));
        let pid = ra["result"]["pid"].as_i64().expect("the server's pid") as i32;
        assert_eq!(rb["result"]["pid"], ra["result"]["pid"], "one process for both windows");
        assert_eq!(std::fs::read_to_string(&log).unwrap().matches("start").count(), 1);
        let alive = || unsafe { libc::kill(pid, 0) } == 0;
        a.bye();
        drop(a);
        thread::sleep(Duration::from_millis(400));
        assert!(alive(), "window B still uses it");
        assert_eq!(mcp_ask(&b, &def, 1, call(10))["id"], 10);
        b.bye();
        drop(b);
        wait_for("the server to stop with the last window", || !alive());
        done.recv_timeout(Duration::from_secs(5)).expect("the supervisor ends too");
    }

    #[test]
    fn the_shared_servers_are_listed_with_their_windows_and_restart_on_request() {
        let _own = share::tests::HUB_TEST.lock_ok();
        let p = paths("mcp-manage");
        let done = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());
        let (def, log) = share::tests::server("sup-manage");
        let (a, ra) = client(&p, std::process::id());
        let (b, _rb) = client(&p, std::process::id());
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}});
        let call = |id: u64| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {}});
        mcp_ask(&a, &def, 1, init.clone());
        mcp_ask(&b, &def, 1, init);
        let pid = mcp_ask(&a, &def, 1, call(2))["result"]["pid"].as_u64().unwrap();
        let listed = |rx: &mpsc::Receiver<Msg>| {
            a.ask_status();
            loop {
                if let Msg::Status(v) = rx.recv_timeout(Duration::from_secs(5)).expect("a status") {
                    break v["mcp"].as_array().unwrap().iter().find(|m| m["command"] == def.command.as_str()).cloned();
                }
            }
        };
        let row = listed(&ra).expect("the server is listed");
        assert_eq!((row["pid"].as_u64(), row["windows"].as_u64()), (Some(pid), Some(2)), "{row}");
        a.restart_mcp(row["key"].as_str().unwrap());
        // The next message starts it again, as a new process.
        let t = Instant::now();
        let new = loop {
            let r = mcp_ask(&b, &def, 1, call(3));
            if let Some(n) = r["result"]["pid"].as_u64().filter(|n| *n != pid) {
                break n;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "never restarted: {r}");
            thread::sleep(Duration::from_millis(50));
        };
        assert_ne!(new, pid);
        assert_eq!(std::fs::read_to_string(&log).unwrap().matches("start").count(), 2);
        a.bye();
        b.bye();
        drop((a, b));
        done.recv_timeout(Duration::from_secs(5)).expect("the supervisor ends");
    }

    #[test]
    fn a_message_in_flight_when_the_supervisor_goes_away_gets_an_error_and_later_ones_are_handed_back() {
        let p = paths("mcp-lost");
        let _ = serve_in_background(&p, fast());
        wait_for("the socket", || p.sock().exists());
        let (def, _) = share::tests::server("sup-lost");
        let (c, _r) = client(&p, std::process::id());
        let hang = json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"hang": true}});
        let (tx, rx) = mpsc::channel();
        c.mcp(&def, 1, hang, Box::new(move |v| tx.send(v).unwrap())).map_err(drop).unwrap();
        // The server never answers `hang`; closing our side while it waits must not leave the caller waiting too.
        let inner = c.out.lock().unwrap().try_clone().unwrap();
        inner.shutdown(std::net::Shutdown::Both).unwrap();
        let v = rx.recv_timeout(Duration::from_secs(5)).expect("an answer, not silence");
        assert_eq!((&v["id"], v["error"].is_object()), (&json!(5), true));
        wait_for("the client to notice", || !c.alive());
        let (tx, _rx) = mpsc::channel::<Value>();
        assert!(c.mcp(&def, 1, json!({"id": 6}), Box::new(move |v| tx.send(v).unwrap())).is_err(), "handed back: run it here");
    }

    #[test]
    fn it_reports_its_state_changes_its_settings_and_leaves_when_asked() {
        let p = paths("manage");
        let done = serve_in_background(&p, Config { grace: Duration::from_secs(60), budget_mb: 2000, ..fast() });
        wait_for("the socket", || p.sock().exists());
        let (c, rx) = client(&p, std::process::id());
        assert!(c.manages() && c.pid() == Some(std::process::id()), "it says so, and who it is");
        c.tabs(vec![json!({"id": 1, "pid": null, "busy": false, "mb": 120, "cwd": "/x", "idle_s": 0, "safe": false})], vec![]);
        let status = |rx: &mpsc::Receiver<Msg>| loop {
            if let Msg::Status(v) = rx.recv_timeout(Duration::from_secs(5)).expect("a status") {
                break v;
            }
        };
        c.ask_status();
        let mut s = status(&rx);
        while s["windows"][0]["mb"] != 120 {
            thread::sleep(Duration::from_millis(30));
            c.ask_status();
            s = status(&rx);
        }
        assert_eq!((s["settings"]["budget"].clone(), s["settings"]["cleanup"].clone()), (json!(true), json!(true)));
        assert_eq!((s["windows"].as_array().unwrap().len(), s["budget_mb"].clone(), s["total_mb"].clone()), (1, json!(2000), json!(120)));
        assert!(s["exe_mtime"].is_u64() && s["pid"] == std::process::id());

        c.set("budget", "paused");
        c.set("grace_s", "always");
        let mut s = status(&rx);
        while s["settings"]["grace_s"] != "always" {
            s = status(&rx);
        }
        assert_eq!((s["settings"]["budget"].clone(), s["settings"]["grace_s"].clone()), (json!(false), json!("always")));
        assert!(s["log"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("budget = paused")), "{s}");

        c.shutdown();
        done.recv_timeout(Duration::from_secs(5)).expect("it leaves when asked, with a window still connected");
        assert!(!p.sock().exists());
    }

    #[test]
    fn a_supervisor_stopped_on_purpose_is_not_started_again_until_allowed() {
        let p = paths("marker");
        assert!(stop_on_purpose(&p).is_ok() && p.stopped_marker().exists());
        let (tx, _rx) = mpsc::channel();
        assert!(Client::connect_or_start(&p, std::process::id(), tx, |m| m).is_none(), "nothing starts it while it is stopped");
        allow_start(&p);
        assert!(!p.stopped_marker().exists());
        assert_eq!((parse_grace("45"), parse_grace("always").unwrap() > Duration::from_secs(86400 * 365), parse_grace("x")), (Some(Duration::from_secs(45)), true, None));
    }

    #[test]
    fn an_old_supervisor_is_only_signalled_when_it_really_is_one() {
        let mut c = sleeper();
        assert!(!terminate_old(c.id()), "a process whose command line is not `... supervisor` is never signalled");
        assert!(c.try_wait().unwrap().is_none());
        let _ = c.kill();
    }

    fn front_tab(id: u64, idle_s: u64) -> Value {
        json!({"id": id, "pid": 1000 + id, "mb": 100, "idle_s": idle_s, "safe": false, "safe_front": true})
    }

    #[test]
    fn the_window_you_type_in_is_in_front_and_a_terminal_that_reports_focus_decides_for_itself() {
        let (a, b, c) = (json!({"focus": null, "input_idle_s": 0}), json!({"focus": null, "input_idle_s": 30}), json!({"focus": null, "input_idle_s": 300}));
        let f = fronts(&[(1, &a), (2, &b), (3, &c)]);
        assert_eq!((f[&1], f[&2], f[&3]), (true, true, false), "within a minute of the newest is still in front; five minutes behind is not");
        let (focused, left) = (json!({"focus": true, "input_idle_s": 900}), json!({"focus": false, "input_idle_s": 0}));
        let f = fronts(&[(1, &focused), (2, &left)]);
        assert_eq!((f[&1], f[&2]), (true, false), "focus beats input recency");
        assert!(fronts(&[(1, &Value::Null)])[&1], "a window that says nothing is never treated as left");
        // An older build's window is in front, but it must not become the reference that makes the others look left.
        let (old, reading) = (Value::Null, json!({"focus": null, "input_idle_s": 90}));
        let f = fronts(&[(1, &old), (2, &reading)]);
        assert_eq!((f[&1], f[&2]), (true, true), "the only window that reports is the newest one there is");
        let garbled = json!({"focus": null, "input_idle_s": u64::MAX});
        assert!(!fronts(&[(1, &a), (2, &garbled)])[&2], "a huge idle time is just a long one (and does not overflow)");
    }

    #[test]
    fn the_idle_sweep_stops_tabs_past_the_limit_and_the_front_tab_only_when_you_are_away() {
        let (here, away, left) = (json!({"focus": null, "input_idle_s": 0}), json!({"focus": null, "input_idle_s": 600}), json!({"focus": false, "input_idle_s": 0}));
        let (t_here, t_fresh, t_old) = (vec![front_tab(1, 900)], vec![front_tab(2, 60)], vec![front_tab(3, 120)]);
        let t_left = vec![front_tab(5, 130)];
        let asks = idle_asks(&[(1, &here, &t_here), (2, &away, &t_fresh), (3, &away, &t_old), (5, &left, &t_left)], 120);
        assert_eq!(asks.iter().map(|a| (a.0, a.1)).collect::<Vec<_>>(), vec![(3, 3), (5, 5)], "not the window in use, not a tab idle less than the limit; a terminal that lost focus counts however recently you typed there");
        assert!(idle_asks(&[(5, &left, &t_left)], 0).is_empty(), "limit 0: never");
        // A tab that is not in front goes past the limit, in any window; the tab in front of the window in use, after five times the limit.
        let bg = |id: u64, idle: u64| json!({"id": id, "pid": 1000 + id, "idle_s": idle, "safe": true, "safe_front": false});
        let (in_use, long_gone) = (json!({"focus": true, "input_idle_s": 0}), json!({"focus": true, "input_idle_s": 600}));
        let (mixed, away_front) = (vec![bg(6, 119), bg(7, 120), front_tab(8, 5000)], vec![front_tab(9, 5000)]);
        let asks = idle_asks(&[(1, &in_use, &mixed), (2, &long_gone, &away_front)], 120);
        assert_eq!(asks.iter().map(|a| (a.0, a.1)).collect::<Vec<_>>(), vec![(1, 7), (2, 9)], "a background tab past the limit; the front tab once you have not typed for five times the limit");
        let tabs = vec![front_tab(1, 0), json!({"id": 2, "pid": 1002, "idle_s": 0, "safe": true, "safe_front": false})];
        let eff = |front| effective_tabs(&tabs, front).iter().map(|t| t["safe"] == true).collect::<Vec<_>>();
        assert_eq!((eff(true), eff(false)), (vec![false, true], vec![true, true]), "the tab in front becomes stoppable only in a window you left");
    }

    #[test]
    fn the_supervisor_measures_what_windows_and_their_tabs_hold_and_tells_every_window() {
        let p = paths("measure");
        let _ = serve_in_background(&p, Config { measure: true, ..fast() });
        wait_for("the socket", || p.sock().exists());
        let (a, ra) = client(&p, std::process::id());
        let me = std::process::id();
        // This test process stands in for a tab's process: it holds megabytes.
        a.tabs(vec![json!({"id": 1, "pid": me, "idle_s": 0})], vec![]);
        let held = |ra: &mpsc::Receiver<Msg>| loop {
            if let Msg::Peers(peers) = ra.recv_timeout(Duration::from_secs(10)).expect("peers with numbers")
                && let Some(w) = peers.iter().find(|i| i.ui_mb > 0)
                && w.tabs.iter().any(|t| t.mb > 0)
            {
                return (w.ui_mb, w.tabs[0].mb);
            }
        };
        let (ui, tab) = held(&ra);
        assert!(ui > 0 && tab > 0);
        // The window saying more about its tabs (without numbers) does not wipe what was measured.
        a.tabs(vec![json!({"id": 1, "pid": me, "idle_s": 30})], vec![]);
        thread::sleep(Duration::from_millis(100));
        let (_, again) = held(&ra);
        assert!(again > 0);
    }

    #[test]
    fn over_budget_with_nothing_safe_to_stop_the_windows_are_told() {
        let p = paths("notice");
        let _ = serve_in_background(&p, Config { budget_mb: 500, ..fast() });
        wait_for("the socket", || p.sock().exists());
        let (a, ra) = client(&p, std::process::id());
        let (b, rb) = client(&p, std::process::id());
        a.tabs(vec![tab(1, 900, 0, false)], vec![]);
        b.tabs(vec![tab(2, 10, 0, false)], vec![]);
        loop {
            if let Msg::Notice(text) = ra.recv_timeout(Duration::from_secs(5)).expect("a notice") {
                assert!(text.contains("910") && text.contains("500") && text.contains("no tab is safe"), "{text}");
                break;
            }
        }
        let heard_by_b = std::iter::from_fn(|| rb.recv_timeout(Duration::from_secs(5)).ok()).any(|m| matches!(m, Msg::Notice(_)));
        assert!(heard_by_b, "every window hears it");
    }

    #[test]
    fn a_window_that_cannot_use_the_shared_supervisor_has_one_of_its_own_with_the_same_numbers() {
        let (tx, rx) = mpsc::channel();
        let c = Client::private(std::process::id(), tx, |m| m).expect("a private supervisor");
        assert!(c.is_private() && c.manages() && c.alive());
        c.tabs(vec![json!({"id": 1, "pid": null, "idle_s": 0})], vec![]);
        loop {
            if let Msg::Peers(peers) = rx.recv_timeout(Duration::from_secs(10)).expect("the window is measured")
                && peers.iter().any(|i| i.pid == std::process::id() && i.ui_mb > 0)
            {
                break;
            }
        }
        drop(c); // closing the connection ends the supervisor behind it (checked by the thread's grace, not here)
    }

    #[test]
    fn a_window_you_left_is_asked_to_stop_its_front_tab_with_no_budget_and_the_one_you_use_is_not() {
        let p = paths("front");
        let _ = serve_in_background(&p, Config { idle_s: 120, ..fast() });
        wait_for("the socket", || p.sock().exists());
        let (a, ra) = client(&p, std::process::id());
        let (b, rb) = client(&p, std::process::id());
        a.window(None, 0);
        a.tabs(vec![front_tab(1, 600)], vec![]);
        b.window(None, 300);
        b.tabs(vec![front_tab(2, 600)], vec![]);
        loop {
            if let Msg::Suspend { tab, front, .. } = rb.recv_timeout(Duration::from_secs(5)).expect("a request to B") {
                assert_eq!((tab, front), (2, true));
                break;
            }
        }
        assert!(!std::iter::from_fn(|| ra.try_recv().ok()).any(|m| matches!(m, Msg::Suspend { .. })), "A is the window in use");
        // Asked once, not again at every check while the owner has not answered yet (5 checks = 1 s here).
        thread::sleep(Duration::from_millis(700));
        assert!(!std::iter::from_fn(|| rb.try_recv().ok()).any(|m| matches!(m, Msg::Suspend { .. })), "no second request inside the cooldown");
        loop {
            if let Msg::Suspend { tab, .. } = rb.recv_timeout(Duration::from_secs(5)).expect("asked again after the cooldown") {
                assert_eq!(tab, 2);
                break;
            }
        }
        // A window whose terminal has focus is never asked, with a window that IS asked next to it as the proof that the
        // supervisor was working on this very report.
        let (c, rc) = client(&p, std::process::id());
        let (d, rd) = client(&p, std::process::id());
        c.window(Some(true), 300); // 300 s of 600 (five times the limit)
        c.tabs(vec![front_tab(7, 900)], vec![]);
        d.window(Some(false), 0);
        d.tabs(vec![front_tab(8, 900)], vec![]);
        loop {
            if let Msg::Suspend { tab, .. } = rd.recv_timeout(Duration::from_secs(5)).expect("a request to D") {
                assert_eq!(tab, 8);
                break;
            }
        }
        thread::sleep(Duration::from_millis(600));
        assert!(!std::iter::from_fn(|| rc.try_recv().ok()).any(|m| matches!(m, Msg::Suspend { .. })), "a focused terminal is left alone");
    }

    #[test]
    fn nothing_is_stopped_while_the_supervisor_is_paused() {
        let p = paths("front-paused");
        let _ = serve_in_background(&p, Config { enforce: false, idle_s: 120, ..fast() });
        wait_for("the socket", || p.sock().exists());
        let (a, _ra) = client(&p, std::process::id());
        let (b, rb) = client(&p, std::process::id());
        a.window(None, 0);
        a.tabs(vec![front_tab(1, 600)], vec![]);
        b.window(None, 600);
        b.tabs(vec![front_tab(2, 600)], vec![]);
        thread::sleep(Duration::from_millis(900));
        assert!(!std::iter::from_fn(|| rb.try_recv().ok()).any(|m| matches!(m, Msg::Suspend { .. })), "a paused supervisor stops nothing");
    }
}
