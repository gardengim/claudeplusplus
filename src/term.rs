//! Terminals with a pty of their own, for you (`/term`) and for the agent.
//!
//! A `Term` is a shell (or any command) on a pty with the `vt100` emulator reading its output, so what is on its screen is
//! known at any moment: drawn for you, or handed to the agent as text. The agent gets them as a small MCP server that
//! claude++ hosts itself, inside the process (the `sdkMcpServers` route `share.rs` uses): `term_open`, `term_send`,
//! `term_screen`, `term_list`, `term_close`. That is what Claude Code's own Bash tool cannot do: a REPL, `vim`, `ssh`, a
//! debugger, a dev server that keeps running, anything that wants a terminal.
//!
//! Who can see what: a terminal the agent opens is the agent's and you can watch and type in it. One you open is yours and
//! the agent cannot see it until you share it (`s` in the `/term` list). Every tool call still goes through Claude Code's
//! permission prompt like any other tool.
//!
//! A tab owns its terminals, so they survive a restart of the `claude` process behind it (`/cd`, `/profile`) and go with
//! the tab. Nothing is running, and the tools are not even offered to the model, until `/config` → "Agent terminal" is on.

use crate::pane::{Replies, draw_screen, encode, nz, open_pty, winsize};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

/// Called (from the reader thread) when a terminal someone is looking at has new output, or has ended.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Terminals one tab may hold at once.
const MAX_TERMS: usize = 6;
/// Lines of history kept per terminal (the agent can ask for them, `lines`).
const SCROLLBACK: usize = 2000;
/// The most history lines one `term_screen` call returns.
const MAX_LINES: usize = 500;
/// What the agent's terminals measure until you resize one by looking at it.
const DEFAULT_SIZE: (u16, u16) = (40, 120);
const SETTLE: Duration = Duration::from_millis(300);
const MAX_WAIT: Duration = Duration::from_secs(60);

struct Inner {
    parser: vt100::Parser<Replies>,
    /// Bytes read so far, and when the last arrived: "did it answer, and has it gone quiet".
    bytes: u64,
    last: Instant,
    size: (u16, u16),
}

pub struct Term {
    pub name: String,
    /// What was started, for the list (`$SHELL` when it was just a shell).
    pub command: String,
    pub by_agent: bool,
    /// The agent may see and drive it. Always true for one it opened.
    pub shared: AtomicBool,
    inner: Mutex<Inner>,
    writes: Sender<Vec<u8>>,
    child: Mutex<Option<Child>>,
    master: OwnedFd,
    watch: Arc<AtomicBool>,
    wake: Wake,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Term {
    fn spawn(name: &str, command: Option<&str>, cwd: &str, by_agent: bool, size: (u16, u16), watch: Arc<AtomicBool>, wake: Wake) -> Result<Arc<Term>, String> {
        let shell = std::env::var("SHELL").ok().filter(|s| std::path::Path::new(s).is_file()).unwrap_or_else(|| "sh".into());
        let (mut cmd, shown) = match command {
            Some(c) => (crate::shell::command(Some(&shell), c), c.to_string()),
            None => (Command::new(&shell), shell.clone()),
        };
        if std::path::Path::new(cwd).is_dir() {
            cmd.current_dir(cwd);
        }
        let (child, master) = open_pty(cmd, size).map_err(|e| format!("cannot start {shown}: {e}"))?;
        let mut reader = File::from(master.try_clone().map_err(|e| e.to_string())?);
        let mut writer = File::from(master.try_clone().map_err(|e| e.to_string())?);
        // Writes go through a thread: a pasted megabyte, or a program that stops reading, must not block the caller.
        let (writes, queue) = channel::<Vec<u8>>();
        thread::spawn(move || {
            for bytes in queue {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        let t = Arc::new(Term {
            name: name.to_string(),
            command: shown,
            by_agent,
            shared: AtomicBool::new(by_agent),
            inner: Mutex::new(Inner { parser: vt100::Parser::new_with_callbacks(nz(size.0), nz(size.1), SCROLLBACK, Replies::default()), bytes: 0, last: Instant::now(), size }),
            writes,
            child: Mutex::new(Some(child)),
            master,
            watch,
            wake,
        });
        let weak: Weak<Term> = Arc::downgrade(&t);
        thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EIO is how a closed pty reads on macOS
                    Ok(n) => n,
                };
                let Some(t) = weak.upgrade() else { return };
                t.feed(&buf[..n]);
            }
            if let Some(t) = weak.upgrade() {
                lock(&t.inner).last = Instant::now();
                (t.wake)();
            }
        });
        Ok(t)
    }

    fn feed(&self, bytes: &[u8]) {
        let replies = {
            let mut g = lock(&self.inner);
            g.parser.process(bytes);
            g.bytes += bytes.len() as u64;
            g.last = Instant::now();
            std::mem::take(&mut g.parser.callbacks_mut().0)
        };
        if !replies.is_empty() {
            let _ = self.writes.send(replies);
        }
        if self.watch.load(Ordering::Relaxed) {
            (self.wake)();
        }
    }

    pub fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.writes.send(bytes);
        }
    }

    pub fn key(&self, k: KeyEvent) {
        let app = lock(&self.inner).parser.screen().application_cursor();
        self.write(encode(k, app));
    }

    pub fn paste(&self, text: &str) {
        let text = text.replace("\r\n", "\n");
        if lock(&self.inner).parser.screen().bracketed_paste() {
            self.write(format!("\x1b[200~{text}\x1b[201~").into_bytes());
        } else {
            self.write(text.replace('\n', "\r").into_bytes());
        }
    }

    /// Match the terminal to the area it is drawn in.
    pub fn resize(&self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let mut g = lock(&self.inner);
        if g.size == (rows, cols) {
            return;
        }
        g.size = (rows, cols);
        let ws = winsize(rows, cols);
        // SAFETY: TIOCSWINSZ on the master we own; the kernel signals the foreground program with SIGWINCH.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        g.parser.set_size(nz(rows), nz(cols));
    }

    /// Draw the screen into `area`; returns where the cursor is, if it shows one.
    pub fn render(&self, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
        draw_screen(lock(&self.inner).parser.screen(), buf, area)
    }

    /// `None` while it runs, else how it ended.
    pub fn exit(&self) -> Option<String> {
        let mut c = lock(&self.child);
        let status = c.as_mut()?.try_wait().ok()??;
        Some(status.code().map_or_else(|| status.to_string(), |n| format!("exited {n}")))
    }

    pub fn bytes(&self) -> u64 {
        lock(&self.inner).bytes
    }

    /// The screen as text, trailing blanks trimmed. `lines` above the screen's own height reaches back into the history.
    pub fn text(&self, lines: usize) -> String {
        let mut g = lock(&self.inner);
        let screen = g.parser.screen_mut();
        let rows = screen.size().0.get() as usize;
        let cols = screen.size().1.get();
        screen.set_scrollback(usize::MAX);
        let avail = screen.scrollback();
        let mut off = lines.min(MAX_LINES).saturating_sub(rows).min(avail);
        let mut out: Vec<String> = vec![];
        while off > 0 {
            screen.set_scrollback(off);
            out.extend(screen.rows(0, cols).take(off.min(rows)).map(|r| r.trim_end().to_string()));
            off = off.saturating_sub(rows);
        }
        screen.set_scrollback(0);
        out.extend(screen.rows(0, cols).map(|r| r.trim_end().to_string()));
        while out.last().is_some_and(String::is_empty) {
            out.pop();
        }
        out.join("\n")
    }

    fn cursor(&self) -> (u16, u16, (u16, u16)) {
        let g = lock(&self.inner);
        let (r, c) = g.parser.screen().cursor_position();
        (r + 1, c + 1, g.size)
    }

    /// Block until `until` is on the screen, or (without it) the output has gone quiet for a moment after `mark`
    /// bytes were already read; never longer than `timeout`. True when it ended because the wait was met.
    pub fn wait(&self, mark: u64, until: Option<&str>, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            let (quiet, bytes) = {
                let g = lock(&self.inner);
                (g.last.elapsed(), g.bytes)
            };
            let met = match until {
                Some(u) => self.text(MAX_LINES).contains(u),
                // It answered and went quiet, or said nothing for a while: a command that prints nothing is done too.
                None => quiet >= SETTLE && (bytes > mark || start.elapsed() >= SETTLE * 2),
            };
            if met {
                return true;
            }
            if self.exit().is_some() && quiet >= SETTLE {
                return until.is_none();
            }
            if start.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Term {
    /// Closing the tab hangs the shell up like a closed terminal would.
    fn drop(&mut self) {
        if let Some(mut child) = lock(&self.child).take() {
            if matches!(child.try_wait(), Ok(None)) {
                // SAFETY: signal our own child's session; it is the session leader (setsid at start).
                unsafe { libc::kill(-(child.id() as i32), libc::SIGHUP) };
                let _ = child.kill();
            }
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// The terminals of one tab.
pub struct Terms {
    list: Mutex<Vec<Arc<Term>>>,
    cwd: Mutex<String>,
    /// Someone is looking at a terminal (the list or the viewer): output wakes the UI. Off, the reader threads never do.
    pub watch: Arc<AtomicBool>,
    wake: Wake,
}

impl Terms {
    pub fn new(wake: Wake) -> Arc<Terms> {
        Arc::new(Terms { list: Mutex::new(vec![]), cwd: Mutex::new(String::new()), watch: Arc::new(AtomicBool::new(false)), wake })
    }

    /// The folder new terminals start in: the tab's.
    pub fn set_cwd(&self, cwd: &str) {
        *lock(&self.cwd) = cwd.to_string();
    }

    pub fn all(&self) -> Vec<Arc<Term>> {
        lock(&self.list).clone()
    }

    pub fn get(&self, name: &str) -> Option<Arc<Term>> {
        lock(&self.list).iter().find(|t| t.name == name).cloned()
    }

    /// What the agent may use: its own and the ones you shared.
    fn visible(&self, name: &str) -> Result<Arc<Term>, String> {
        match self.get(name) {
            Some(t) if t.shared.load(Ordering::Relaxed) => Ok(t),
            _ => Err(format!("no terminal named {name:?} (term_list shows them)")),
        }
    }

    pub fn open(&self, name: Option<&str>, command: Option<&str>, by_agent: bool, size: Option<(u16, u16)>) -> Result<Arc<Term>, String> {
        let mut list = lock(&self.list);
        list.retain(|t| t.exit().is_none() || Arc::strong_count(t) > 1);
        if list.len() >= MAX_TERMS {
            return Err(format!("{MAX_TERMS} terminals are open in this tab; close one first"));
        }
        let name = match name {
            Some(n) if valid_name(n) => n.to_string(),
            Some(n) => return Err(format!("{n:?} is not a name: 1 to 24 of a-z 0-9 - _")),
            None => (1..).map(|i| format!("{}{i}", if by_agent { "t" } else { "you" })).find(|n| !list.iter().any(|t| &t.name == n)).unwrap_or_default(),
        };
        if list.iter().any(|t| t.name == name) {
            return Err(format!("a terminal named {name:?} exists"));
        }
        let cwd = lock(&self.cwd).clone();
        let t = Term::spawn(&name, command, &cwd, by_agent, size.unwrap_or(DEFAULT_SIZE), self.watch.clone(), self.wake.clone())?;
        list.push(t.clone());
        Ok(t)
    }

    pub fn close(&self, name: &str) -> bool {
        let mut list = lock(&self.list);
        let before = list.len();
        list.retain(|t| t.name != name);
        list.len() != before
    }

    /// One JSON-RPC message from the tab's `claude` for the server `term`. `reply` gets the answer (for a
    /// notification, an empty result), from this thread or, for a tool call that may wait, another.
    pub fn handle(self: &Arc<Self>, msg: &Value, reply: crate::share::Reply) {
        let id = msg.get("id").cloned();
        let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        match (msg["method"].as_str().unwrap_or(""), &id) {
            (_, None) => reply(json!({"jsonrpc": "2.0", "result": {}})),
            ("initialize", _) => reply(ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "term", "version": env!("CARGO_PKG_VERSION")},
            }))),
            ("ping", _) => reply(ok(json!({}))),
            ("tools/list", _) => reply(ok(json!({"tools": tools()}))),
            ("tools/call", _) => {
                let (this, params, rid) = (self.clone(), msg["params"].clone(), id.clone());
                thread::spawn(move || {
                    let (text, error) = match this.call(params["name"].as_str().unwrap_or(""), &params["arguments"]) {
                        Ok(t) => (t, false),
                        Err(e) => (e, true),
                    };
                    reply(json!({"jsonrpc": "2.0", "id": rid, "result": {"content": [{"type": "text", "text": text}], "isError": error}}));
                });
            }
            _ => reply(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})),
        }
    }

    fn call(&self, tool: &str, a: &Value) -> Result<String, String> {
        let arg = |k: &str| a[k].as_str().filter(|s| !s.is_empty());
        let named = || arg("name").ok_or_else(|| "name is required".to_string()).and_then(|n| self.visible(n));
        let timeout = Duration::from_millis(a["timeout_ms"].as_u64().unwrap_or(5000)).min(MAX_WAIT);
        let lines = a["lines"].as_u64().unwrap_or(0) as usize;
        match tool {
            "term_open" => {
                let t = self.open(arg("name"), arg("command"), true, None)?;
                let mark = t.bytes();
                t.wait(mark, None, Duration::from_millis(1500));
                Ok(report(&t, lines, true))
            }
            "term_send" => {
                let t = named()?;
                let mut bytes = arg("text").unwrap_or("").replace("\r\n", "\n").replace('\n', "\r").into_bytes();
                for k in a["keys"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    let key = parse_key(k)?;
                    let app = lock(&t.inner).parser.screen().application_cursor();
                    bytes.extend(encode(key, app));
                }
                if bytes.is_empty() {
                    return Err("nothing to send: give text and/or keys".into());
                }
                let mark = t.bytes();
                t.write(bytes);
                let met = t.wait(mark, arg("until"), timeout);
                Ok(report(&t, lines, met || arg("until").is_none()))
            }
            "term_screen" => {
                let t = named()?;
                let met = arg("until").is_none_or(|u| t.wait(t.bytes(), Some(u), timeout));
                Ok(report(&t, lines, met))
            }
            "term_list" => {
                let rows: Vec<String> = self.all().iter().filter(|t| t.shared.load(Ordering::Relaxed)).map(|t| format!("{}: {} ({})", t.name, t.command, t.exit().unwrap_or_else(|| "running".into()))).collect();
                Ok(if rows.is_empty() { "no terminals".into() } else { rows.join("\n") })
            }
            "term_close" => {
                let t = named()?;
                self.close(&t.name);
                Ok(format!("closed {}", t.name))
            }
            _ => Err(format!("no such tool: {tool}")),
        }
    }
}

fn valid_name(n: &str) -> bool {
    (1..=24).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The screen, then one line saying where the cursor is and whether it still runs.
fn report(t: &Term, lines: usize, met: bool) -> String {
    let (r, c, (rows, cols)) = t.cursor();
    let state = t.exit().unwrap_or_else(|| "running".into());
    let note = if met { "" } else { " · timed out before the wait was met" };
    format!("{}\n--- {} · {state} · cursor row {r} col {c} · {cols}x{rows}{note}", t.text(lines), t.name)
}

/// "Enter", "Ctrl-C", "Alt-x", "Shift-Tab", "F5", "a" as the key event a terminal would deliver.
pub fn parse_key(spec: &str) -> Result<KeyEvent, String> {
    let (mut mods, mut rest) = (KeyModifiers::NONE, spec);
    loop {
        let lower = rest.to_ascii_lowercase();
        let (m, n) = if lower.starts_with("ctrl-") || lower.starts_with("ctrl+") {
            (KeyModifiers::CONTROL, 5)
        } else if lower.starts_with("alt-") || lower.starts_with("alt+") {
            (KeyModifiers::ALT, 4)
        } else if lower.starts_with("shift-") || lower.starts_with("shift+") {
            (KeyModifiers::SHIFT, 6)
        } else if lower.starts_with("c-") && rest.len() > 2 {
            (KeyModifiers::CONTROL, 2)
        } else if lower.starts_with("m-") && rest.len() > 2 {
            (KeyModifiers::ALT, 2)
        } else {
            break;
        };
        mods |= m;
        rest = &rest[n..];
    }
    let code = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" if mods.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        f if f.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=12).contains(&n)) => KeyCode::F(f[1..].parse().unwrap_or(1)),
        _ if rest.chars().count() == 1 => KeyCode::Char(rest.chars().next().unwrap_or(' ')),
        _ => return Err(format!("unknown key {spec:?}: Enter Tab Esc Space Backspace Delete Up Down Left Right Home End PageUp PageDown F1-F12, Ctrl-X, Alt-X, Shift-Tab or one character")),
    };
    Ok(KeyEvent::new(code, mods))
}

fn tools() -> Value {
    let name = json!({"type": "string", "description": "terminal name"});
    let until = json!({"type": "string", "description": "wait until this text is on the screen (up to timeout_ms) instead of until the output goes quiet; pick text your input will cause, not one already shown such as the prompt"});
    let timeout = json!({"type": "integer", "description": "longest wait in ms (default 5000, max 60000)"});
    let lines = json!({"type": "integer", "description": "also return this many lines of history above the screen (max 500)"});
    json!([
        {"name": "term_open", "description": "Open a terminal (a real pty) and return its screen. Use it for what Bash cannot do: REPLs, vim, ssh, debuggers, servers you keep watching. Default: an interactive shell.",
         "inputSchema": {"type": "object", "properties": {"command": {"type": "string", "description": "run this instead of a shell"}, "name": {"type": "string", "description": "1-24 of a-z 0-9 - _ (default t1, t2, ...)"}, "lines": lines}}},
        {"name": "term_send", "description": "Type into a terminal, then return its screen once the output goes quiet. The text is typed first (a newline in it presses Enter), then the keys, which are named: Enter Tab Esc Space Backspace Up Down Left Right Home End PageUp PageDown F1-F12, Ctrl-C, Alt-x.",
         "inputSchema": {"type": "object", "properties": {"name": name, "text": {"type": "string"}, "keys": {"type": "array", "items": {"type": "string"}}, "until": until, "timeout_ms": timeout, "lines": lines}, "required": ["name"]}},
        {"name": "term_screen", "description": "Read a terminal's screen without typing, optionally waiting for text to appear.",
         "inputSchema": {"type": "object", "properties": {"name": name, "until": until, "timeout_ms": timeout, "lines": lines}, "required": ["name"]}},
        {"name": "term_list", "description": "The terminals you can use, with whether they still run.", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "term_close", "description": "Close a terminal and end what runs in it.", "inputSchema": {"type": "object", "properties": {"name": name}, "required": ["name"]}},
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms() -> Arc<Terms> {
        let t = Terms::new(Arc::new(|| {}));
        t.set_cwd("/tmp");
        t
    }

    /// Skipped where a pty cannot be opened (a sandbox).
    fn no_pty() -> bool {
        let none = crate::pane::open_pty(Command::new("true"), (2, 2)).is_err();
        if none {
            eprintln!("skipped: no pty here");
        }
        none
    }

    fn call(t: &Arc<Terms>, tool: &str, args: Value) -> Result<String, String> {
        t.call(tool, &args)
    }

    #[test]
    fn keys_are_named_the_way_people_say_them() {
        let enc = |s: &str| encode(parse_key(s).unwrap(), false);
        assert_eq!(enc("Enter"), b"\r");
        assert_eq!(enc("Ctrl-C"), [3]);
        assert_eq!(enc("ctrl+d"), [4]);
        assert_eq!(enc("Alt-x"), b"\x1bx");
        assert_eq!(enc("Up"), b"\x1b[A");
        assert_eq!(enc("Esc"), b"\x1b");
        assert_eq!(enc("Shift-Tab"), b"\x1b[Z");
        assert_eq!(enc("F5"), b"\x1b[15~");
        assert_eq!(enc("q"), b"q");
        assert!(parse_key("Hyper-Q").is_err() && parse_key("F13").is_err());
    }

    #[test]
    fn a_shell_runs_what_is_typed_and_the_agent_sees_its_output() {
        if no_pty() {
            return;
        }
        let t = terms();
        let open = call(&t, "term_open", json!({"command": "sh"})).unwrap();
        assert!(open.contains("--- t1 · running"), "{open}");
        // "42" appears only in the output, not in the echoed command.
        let out = call(&t, "term_send", json!({"name": "t1", "text": "echo hello-$((40+2))\n", "until": "hello-42"})).unwrap();
        assert!(out.contains("hello-42") && !out.contains("timed out"), "{out}");
        // Ctrl-C ends a running command and the shell takes the next line.
        call(&t, "term_send", json!({"name": "t1", "text": "sleep 30\n"})).unwrap();
        call(&t, "term_send", json!({"name": "t1", "keys": ["Ctrl-C"]})).unwrap();
        let out = call(&t, "term_send", json!({"name": "t1", "text": "echo back-$((1+1))", "keys": ["Enter"], "until": "back-2"})).unwrap();
        assert!(out.contains("back-2"), "{out}");
        assert_eq!(call(&t, "term_close", json!({"name": "t1"})).unwrap(), "closed t1");
        assert!(call(&t, "term_screen", json!({"name": "t1"})).is_err());
    }

    #[test]
    fn a_program_that_wants_a_terminal_works_and_one_that_ends_says_how() {
        if no_pty() {
            return;
        }
        let t = terms();
        // `read` waits for input from a tty: the thing Bash without a pty cannot do.
        call(&t, "term_open", json!({"command": "read -r a; echo got-$a; exit 3", "name": "ask"})).unwrap();
        let out = call(&t, "term_send", json!({"name": "ask", "text": "x7\n", "until": "got-x7"})).unwrap();
        assert!(out.contains("got-x7"), "{out}");
        t.get("ask").unwrap().wait(0, None, Duration::from_secs(3));
        assert_eq!(t.get("ask").unwrap().exit().as_deref(), Some("exited 3"));
        let out = call(&t, "term_screen", json!({"name": "ask"})).unwrap();
        assert!(out.contains("exited 3"), "{out}");
    }

    #[test]
    fn history_above_the_screen_can_be_asked_for() {
        if no_pty() {
            return;
        }
        let t = terms();
        call(&t, "term_open", json!({"command": "seq 1 100; sleep 5", "name": "seq"})).unwrap();
        t.get("seq").unwrap().wait(0, Some("100"), Duration::from_secs(5));
        let rows = |s: &str| s.lines().take_while(|l| !l.starts_with("---")).map(str::to_string).collect::<Vec<_>>();
        let screen = rows(&call(&t, "term_screen", json!({"name": "seq"})).unwrap());
        assert!(screen.len() <= 40 && !screen.contains(&"1".to_string()), "the screen alone has the tail only: {screen:?}");
        let all = rows(&call(&t, "term_screen", json!({"name": "seq", "lines": 500})).unwrap());
        let want: Vec<String> = (1..=100).map(|n| n.to_string()).collect();
        assert_eq!(all, want, "every line, in order, once");
    }

    #[test]
    fn your_terminals_are_hidden_from_the_agent_until_shared_and_the_count_is_capped() {
        if no_pty() {
            return;
        }
        let t = terms();
        let mine = t.open(Some("mine"), Some("sleep 5"), false, Some((10, 40))).unwrap();
        assert!(call(&t, "term_screen", json!({"name": "mine"})).is_err());
        assert_eq!(call(&t, "term_list", json!({})).unwrap(), "no terminals");
        mine.shared.store(true, Ordering::Relaxed);
        assert!(call(&t, "term_screen", json!({"name": "mine"})).unwrap().contains("--- mine"));
        assert!(call(&t, "term_list", json!({})).unwrap().starts_with("mine: sleep 5 (running)"));
        for i in 0..MAX_TERMS - 1 {
            t.open(Some(&format!("x{i}")), Some("sleep 5"), true, Some((5, 20))).unwrap();
        }
        assert!(t.open(None, Some("sleep 5"), true, None).err().unwrap().contains("close one first"));
        assert!(t.open(Some("Bad Name"), None, true, None).is_err());
        assert!(t.open(Some("mine"), None, true, None).is_err());
    }

    #[test]
    fn the_mcp_side_answers_the_handshake_lists_the_tools_and_runs_a_call() {
        if no_pty() {
            return;
        }
        let t = terms();
        let ask = |msg: Value| {
            let (tx, rx) = channel();
            t.handle(&msg, Box::new(move |v| tx.send(v).unwrap()));
            rx.recv_timeout(Duration::from_secs(10)).unwrap()
        };
        let init = ask(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}));
        assert_eq!(init["result"]["serverInfo"]["name"], "term");
        assert_eq!(init["id"], 1);
        assert_eq!(ask(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})), json!({"jsonrpc": "2.0", "result": {}}));
        let list = ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["term_open", "term_send", "term_screen", "term_list", "term_close"]);
        let call = ask(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "term_open", "arguments": {"command": "echo up-$((6*7))"}}}));
        let text = call["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("up-42") && call["result"]["isError"] == false && call["id"] == 3, "{call}");
        let bad = ask(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "term_screen", "arguments": {"name": "nope"}}}));
        assert_eq!(bad["result"]["isError"], true);
        assert_eq!(ask(json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}))["error"]["code"], -32601);
    }
}
