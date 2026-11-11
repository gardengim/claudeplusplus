//! The real binary in a pseudo-terminal, its output read by a terminal emulator (vt100), which keeps the LAST mouse
//! mode like Ghostty does. What the unit tests cannot see: what the terminal is told while claude++ runs and whether it
//! is left clean. (A drag that no terminal reported, and a shift capture left on after exit, were found by hand.)

use std::ffi::CString;
use std::io::Read;
use std::num::NonZeroU16;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// Collects what the program puts on the clipboard (OSC 52; vt100 does not pass that one on, so the bytes are scanned).
#[derive(Default)]
struct Quiet {
    clipboard: Vec<String>,
    raw: Vec<u8>,
    /// Whether the program last asked for focus-in/out reports (what the idle sweep of the supervisor relies on).
    focus_reports: Option<bool>,
}
impl vt100::Callbacks for Quiet {}
impl Quiet {
    fn absorb(&mut self, chunk: &[u8]) {
        for w in chunk.windows(8) {
            match w {
                b"\x1b[?1004h" => self.focus_reports = Some(true),
                b"\x1b[?1004l" => self.focus_reports = Some(false),
                _ => {}
            }
        }
        self.raw.extend_from_slice(chunk);
        const START: &[u8] = b"\x1b]52;c;";
        while let Some(i) = self.raw.windows(START.len()).position(|w| w == START) {
            let Some(len) = self.raw[i + START.len()..].iter().position(|&c| c == 7) else {
                self.raw.drain(..i); // the rest of it has not arrived
                return;
            };
            let data = self.raw[i + START.len()..i + START.len() + len].to_vec();
            self.clipboard.push(unbase64(&data));
            self.raw.drain(..i + START.len() + len);
        }
        let keep = self.raw.len().saturating_sub(START.len());
        self.raw.drain(..keep);
    }
}

fn unbase64(b: &[u8]) -> String {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for v in b.iter().filter_map(|&c| val(c)) {
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn spawn(home: &std::path::Path, args: &[&str]) -> (OwnedFd, i32) {
    spawn_with(home, args, &[])
}

fn spawn_with(home: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> (OwnedFd, i32) {
    let ws = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
    let mut master = 0;
    // SAFETY: forkpty forks; the child only sets the environment and execs.
    let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &ws as *const libc::winsize as *mut libc::winsize) };
    assert!(pid >= 0, "forkpty");
    if pid == 0 {
        let sup = home.join("sup");
        for (k, v) in [("HOME", home), ("XDG_CONFIG_HOME", home), ("CLAUDEPP_SUP_DIR", &sup), ("TMPDIR", home)] {
            unsafe { libc::setenv(CString::new(k).unwrap().as_ptr(), CString::new(v.to_str().unwrap()).unwrap().as_ptr(), 1) };
        }
        for (k, v) in [("OPENVIKING_MEMORY_ENABLED", "0"), ("TERM", "xterm-256color")].into_iter().chain(env.iter().copied()) {
            unsafe { libc::setenv(CString::new(k).unwrap().as_ptr(), CString::new(v).unwrap().as_ptr(), 1) };
        }
        let _ = std::env::set_current_dir(home);
        let exe = CString::new(env!("CARGO_BIN_EXE_claudeplusplus")).unwrap();
        let args: Vec<CString> = args.iter().map(|a| CString::new(*a).unwrap()).collect();
        let argv: Vec<*const libc::c_char> = std::iter::once(exe.as_ptr()).chain(args.iter().map(|a| a.as_ptr())).chain([std::ptr::null()]).collect();
        unsafe { libc::execv(exe.as_ptr(), argv.as_ptr()) };
        unsafe { libc::_exit(127) };
    }
    // SAFETY: master is a fresh descriptor owned by nobody else.
    (unsafe { OwnedFd::from_raw_fd(master) }, pid)
}

/// Read what the program writes for `secs` (or until it closes the terminal), answering the questions it asks the
/// terminal, and feed it to the emulator. True when the program closed the terminal.
fn emulator() -> vt100::Parser<Quiet> {
    vt100::Parser::new_with_callbacks(NonZeroU16::new(30).unwrap(), NonZeroU16::new(100).unwrap(), 0, Quiet::default())
}

fn pump(fd: &OwnedFd, parser: &mut vt100::Parser<Quiet>, secs: f64) -> bool {
    use std::os::fd::AsRawFd;
    let end = Instant::now() + Duration::from_secs_f64(secs);
    let mut file = unsafe { std::fs::File::from_raw_fd(libc::dup(fd.as_raw_fd())) };
    while Instant::now() < end {
        let mut p = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, 30) } <= 0 {
            continue;
        }
        let mut buf = [0u8; 65536];
        let n = match file.read(&mut buf) {
            Ok(0) | Err(_) => return true,
            Ok(n) => n,
        };
        let chunk = &buf[..n];
        let has = |s: &[u8]| chunk.windows(s.len()).any(|w| w == s);
        let mut reply = Vec::new();
        if has(b"\x1b[6n") {
            reply.extend_from_slice(b"\x1b[1;1R");
        }
        if has(b"\x1b]11;?") {
            reply.extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x07");
        }
        if has(b"\x1b[c") || has(b"\x1b[0c") || has(b"\x1b]11;?") {
            reply.extend_from_slice(b"\x1b[?62;c");
        }
        if !reply.is_empty() {
            unsafe { libc::write(fd.as_raw_fd(), reply.as_ptr().cast(), reply.len()) };
        }
        parser.callbacks_mut().absorb(chunk);
        parser.process(chunk);
    }
    false
}

#[test]
fn the_terminal_is_told_to_report_drags_and_is_left_exactly_as_it_was() {
    let home = std::env::temp_dir().join(format!("cpp-terminal-it-{}", std::process::id()));
    cleanup(&home);
    std::fs::create_dir_all(&home).unwrap();
    let (fd, pid) = spawn(&home, &[]);
    let _reap = Reap(pid, Some(home.clone()));
    let mut parser = emulator();
    // Until the first frame is drawn and the modes are on (a cold first start can take a few seconds).
    let started = Instant::now();
    loop {
        pump(&fd, &mut parser, 0.3);
        let s = parser.screen();
        if s.alternate_screen() && s.mouse_protocol_mode() != vt100::MouseProtocolMode::None {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "claude++ never took the terminal: {:?}", s.contents());
    }
    let s = parser.screen();
    assert_eq!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::ButtonMotion, "drags must be reported (the last of 1000/1002 wins)");
    assert_eq!(s.mouse_protocol_encoding(), vt100::MouseProtocolEncoding::Sgr);
    assert!(s.bracketed_paste(), "paste arrives as a paste, not as typing");
    assert_eq!(parser.callbacks().focus_reports, Some(true), "focus reports must be on, or the supervisor never learns the window is in the background");

    // Asked to end, as a closed window or `kill` does: everything is handed back.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let closed = pump(&fd, &mut parser, 10.0);
    let mut status = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    assert!(closed || waited == pid, "claude++ did not end after SIGTERM");
    let s = parser.screen();
    assert!(!s.alternate_screen(), "the alternate screen is left");
    assert_eq!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::None, "mouse reporting is off again");
    assert_eq!(s.mouse_protocol_encoding(), vt100::MouseProtocolEncoding::Default);
    assert!(!s.bracketed_paste(), "bracketed paste is off again");
    assert!(!s.hide_cursor(), "the cursor is visible again");
    assert_eq!(parser.callbacks().focus_reports, Some(false), "focus reports are off again");
    if waited != pid {
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
    cleanup(&home);
}

/// What a terminal does with the mouse: it sends a drag only when the program asked for button-motion reports.
fn mouse(fd: &OwnedFd, parser: &mut vt100::Parser<Quiet>, kind: &str, shift: bool, col: u16, row: u16) {
    use std::os::fd::AsRawFd;
    let mode = parser.screen().mouse_protocol_mode();
    let base = if shift { 4 } else { 0 };
    let (code, end) = match kind {
        "down" => (base, 'M'),
        "up" => (base, 'm'),
        _ => {
            assert!(matches!(mode, vt100::MouseProtocolMode::ButtonMotion | vt100::MouseProtocolMode::AnyMotion), "a terminal in {mode:?} sends no drag");
            (32 + base, 'M')
        }
    };
    assert!(mode != vt100::MouseProtocolMode::None, "mouse reporting is off");
    let ev = format!("\x1b[<{code};{};{}{end}", col + 1, row + 1);
    unsafe { libc::write(fd.as_raw_fd(), ev.as_ptr().cast(), ev.len()) };
    pump(fd, parser, 0.15);
}

fn find(parser: &vt100::Parser<Quiet>, text: &str) -> (u16, u16) {
    let s = parser.screen();
    for (r, line) in s.rows(0, 100).enumerate() {
        if let Some(i) = line.find(text) {
            return (r as u16, line[..i].chars().count() as u16);
        }
    }
    panic!("{text:?} is not on the screen:\n{}", s.contents());
}

fn marked(parser: &vt100::Parser<Quiet>, row: u16) -> usize {
    let s = parser.screen();
    (0..100).filter(|&c| s.cell(row, c).is_some_and(|x| x.bgcolor() == vt100::Color::Idx(24))).count()
}

fn session_home(tag: &str) -> std::path::PathBuf {
    let id = "5f0c1d2e-3a4b-4c5d-8e6f-7a8b9c0d1e2f";
    let line = |t: &str, v: &str| format!(r#"{{"type":"{t}","uuid":"{id}","sessionId":"{id}","timestamp":"2026-11-10T00:00:00.000Z","cwd":"CWD",{v}}}"#);
    let user = line("user", r#""message":{"role":"user","content":"say something"}"#);
    let asst = line("assistant", r#""message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-haiku-4-5","content":[{"type":"text","text":"alpha beta gamma delta epsilon zeta"}],"usage":{"input_tokens":1,"output_tokens":1}}"#);
    home_with(tag, &format!("{user}\n{asst}\n"))
}

/// A home whose one session is `body` (`CWD` in it becomes the folder).
fn home_with(tag: &str, body: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("cpp-terminal-it-{tag}-{}", std::process::id()));
    cleanup(&home);
    std::fs::create_dir_all(&home).unwrap();
    let home = home.canonicalize().unwrap();
    let cwd = home.to_str().unwrap();
    let dir = home.join(".claude/projects").join(cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("5f0c1d2e-3a4b-4c5d-8e6f-7a8b9c0d1e2f.jsonl"), body.replace("CWD", cwd)).unwrap();
    home
}

#[test]
fn dragging_in_the_conversation_highlights_as_the_pointer_moves_and_copies_with_or_without_shift() {
    let home = session_home("drag");
    let (fd, pid) = spawn(&home, &["-c"]);
    let _reap = Reap(pid, Some(home.clone()));
    let mut parser = emulator();
    let started = Instant::now();
    while !parser.screen().contents().contains("alpha beta gamma") || parser.screen().mouse_protocol_mode() == vt100::MouseProtocolMode::None {
        pump(&fd, &mut parser, 0.3);
        assert!(started.elapsed() < Duration::from_secs(20), "the conversation never appeared:\n{}", parser.screen().contents());
    }
    pump(&fd, &mut parser, 0.5);
    for shift in [false, true] {
        let (row, col) = find(&parser, "alpha");
        parser.callbacks_mut().clipboard.clear();
        mouse(&fd, &mut parser, "down", shift, col, row);
        let mut seen = Vec::new();
        for step in 1..=9u16 {
            mouse(&fd, &mut parser, "drag", shift, col + step, row);
            // Each step shows at once on an idle machine; on a busy one, give the frame time rather than fail on timing.
            let t = Instant::now();
            while marked(&parser, row) != step as usize + 1 && t.elapsed() < Duration::from_secs(3) {
                pump(&fd, &mut parser, 0.05);
            }
            seen.push(marked(&parser, row));
        }
        assert_eq!(seen, (2..=10).collect::<Vec<_>>(), "the highlight grows with every step of the pointer (shift: {shift})");
        assert!(parser.callbacks().clipboard.last().is_some_and(|t| t == "alpha beta"), "copied while dragging: {:?}", parser.callbacks().clipboard);
        mouse(&fd, &mut parser, "up", shift, col + 9, row);
        pump(&fd, &mut parser, 0.3);
        assert!(parser.screen().contents().contains("copied 10 characters"), "{}", parser.screen().contents());
        // A plain click elsewhere drops it (a shift+click would move its end instead).
        mouse(&fd, &mut parser, "down", false, col, row + 3);
        mouse(&fd, &mut parser, "up", false, col, row + 3);
        assert_eq!(marked(&parser, row), 0, "a click clears the highlight");
    }
    unsafe { libc::kill(pid, libc::SIGTERM) };
    pump(&fd, &mut parser, 5.0);
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    cleanup(&home);
}

#[test]
fn a_click_in_the_message_box_puts_the_cursor_under_the_pointer() {
    use std::os::fd::AsRawFd;
    let home = session_home("click");
    let (fd, pid) = spawn(&home, &[]);
    let _reap = Reap(pid, Some(home.clone()));
    let mut parser = emulator();
    let started = Instant::now();
    while parser.screen().mouse_protocol_mode() == vt100::MouseProtocolMode::None {
        pump(&fd, &mut parser, 0.3);
        assert!(started.elapsed() < Duration::from_secs(20), "claude++ never took the terminal");
    }
    pump(&fd, &mut parser, 1.0);
    let text = b"hello big world";
    unsafe { libc::write(fd.as_raw_fd(), text.as_ptr().cast(), text.len()) };
    pump(&fd, &mut parser, 0.5);
    let (row, col) = find(&parser, "hello big world");
    mouse(&fd, &mut parser, "down", false, col + 6, row);
    mouse(&fd, &mut parser, "up", false, col + 6, row);
    assert_eq!(parser.screen().cursor_position(), (row, col + 6), "the cursor is on the b of big");
    // dragging in the box selects, as the keyboard does
    mouse(&fd, &mut parser, "down", false, col + 6, row);
    mouse(&fd, &mut parser, "drag", false, col + 9, row);
    assert!(parser.callbacks().clipboard.last().is_some_and(|t| t == "big"), "{:?}", parser.callbacks().clipboard);
    mouse(&fd, &mut parser, "up", false, col + 9, row);
    unsafe { libc::kill(pid, libc::SIGTERM) };
    pump(&fd, &mut parser, 5.0);
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    cleanup(&home);
}

/// The supervisor claude++ started for the test would linger for its grace period: ask it to leave, then remove the folder.
fn cleanup(home: &std::path::Path) {
    use std::io::Write;
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(home.join("sup/sup.sock")) {
        let _ = writeln!(s, r#"{{"t":"hello","v":1,"pid":{}}}"#, std::process::id());
        let _ = writeln!(s, r#"{{"t":"shutdown"}}"#);
        std::thread::sleep(Duration::from_millis(300));
    }
    let _ = std::fs::remove_dir_all(home);
}

/// Kills claude++ when a test ends, however it ends: an assertion that fails must not leave an instance, or the
/// supervisor it started, running. With a home, that supervisor is asked to leave and the folder removed.
struct Reap(i32, Option<std::path::PathBuf>);
impl Drop for Reap {
    fn drop(&mut self) {
        // SAFETY: a signal to a child of this process; already gone is harmless.
        unsafe { libc::kill(self.0, libc::SIGKILL) };
        if let Some(home) = &self.1 {
            cleanup(home);
        }
    }
}

/// The pid in a supervisor's welcome, saying hello as this process (so ask only once nothing else needs it to be alone).
fn welcome_pid(home: &std::path::Path) -> Option<u64> {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(home.join("sup/sup.sock")).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    writeln!(s, r#"{{"t":"hello","v":1,"pid":{}}}"#, std::process::id()).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(s).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v["pid"].as_u64()
}

#[test]
fn a_window_replaces_a_supervisor_from_an_older_build_when_it_is_the_only_one_attached() {
    let home = session_home("old");
    // The same program, but a day older: what a supervisor left running from before the last build looks like.
    let old = home.join("claudeplusplus-old");
    std::fs::copy(env!("CARGO_BIN_EXE_claudeplusplus"), &old).unwrap();
    let day_ago = std::time::SystemTime::now() - Duration::from_secs(86400);
    std::fs::File::options().write(true).open(&old).unwrap().set_modified(day_ago).unwrap();
    let mut stale = std::process::Command::new(&old)
        .arg("supervisor")
        .env("CLAUDEPP_SUP_DIR", home.join("sup"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _kill_stale = Reap(stale.id() as i32, None);
    let t = Instant::now();
    while !home.join("sup/sup.sock").exists() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(home.join("sup/sup.sock").exists(), "the old supervisor never listened: {:?}", stale.try_wait());

    let (fd, pid) = spawn(&home, &[]);
    let _reap = Reap(pid, Some(home.clone()));
    let mut term = emulator();
    pump(&fd, &mut term, 5.0);
    assert!(stale.try_wait().unwrap().is_some(), "the old supervisor is still running");
    let t = Instant::now();
    let new = loop {
        match welcome_pid(&home) {
            Some(p) if p != stale.id() as u64 => break p,
            _ if t.elapsed() > Duration::from_secs(5) => panic!("no new supervisor took over"),
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert_ne!(new, stale.id() as u64);
    unsafe { libc::kill(pid, libc::SIGTERM) };
    pump(&fd, &mut term, 1.5); // the program cannot finish leaving until its last output is read
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

/// Pump until `text` is on the screen; the seconds it took, or None at `limit`.
fn until(fd: &OwnedFd, parser: &mut vt100::Parser<Quiet>, text: &str, limit: f64) -> Option<f64> {
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < limit {
        if parser.screen().contents().contains(text) {
            return Some(t.elapsed().as_secs_f64());
        }
        pump(fd, parser, 0.02);
    }
    None
}

/// `turns` rounds of: a question, a tool call, a 3 KB tool result, an answer (what a long agent session looks like).
fn long_session(turns: usize) -> String {
    let (mut out, mut parent) = (String::new(), "null".to_string());
    let mut n = 0;
    let mut add = |kind: &str, message: String| {
        n += 1;
        let id = format!("00000000-0000-4000-8000-{n:012}");
        out += &format!(r#"{{"type":"{kind}","uuid":"{id}","parentUuid":{parent},"sessionId":"5f0c1d2e-3a4b-4c5d-8e6f-7a8b9c0d1e2f","timestamp":"2026-11-10T00:00:00.000Z","cwd":"CWD","message":{message}}}"#);
        out.push('\n');
        parent = format!("\"{id}\"");
    };
    for t in 0..turns {
        let marker = if t + 1 == turns { "TAILMARK" } else { "mid" };
        add("user", format!(r#"{{"role":"user","content":"question {t} about the build, please look at it"}}"#));
        add("assistant", format!(r#"{{"id":"m{t}a","type":"message","role":"assistant","model":"claude-haiku-4-5","content":[{{"type":"tool_use","id":"tu{t}","name":"Bash","input":{{"command":"cargo test -p part{t}"}}}}],"usage":{{"input_tokens":1,"output_tokens":1}}}}"#));
        let result = (0..60).map(|l| format!("line {l} of the output of step {t}: compiling a crate named something_{l} v0.{l}.0")).collect::<Vec<_>>().join("\\n");
        add("user", format!(r#"{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu{t}","content":"{result}"}}]}}"#));
        add("assistant", format!(r#"{{"id":"m{t}b","type":"message","role":"assistant","model":"claude-haiku-4-5","content":[{{"type":"text","text":"Step {t} is done, {marker}: the crates compiled and the tests passed. Next I will look at the following part."}}],"usage":{{"input_tokens":1,"output_tokens":1}}}}"#));
    }
    out
}

fn rss_mb(pid: i32) -> f64 {
    let o = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

#[test]
fn a_long_tool_heavy_session_opens_fast_stays_small_and_answers_keys_at_once() {
    let turns: usize = std::env::var("CPP_TURNS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
    let body = long_session(turns);
    let home = home_with("big", &body);
    let (fd, pid) = spawn(&home, &["-c"]);
    let _reap = Reap(pid, Some(home.clone()));
    let mut term = emulator();
    let open = until(&fd, &mut term, "TAILMARK", 60.0).expect("the end of the session never appeared");
    pump(&fd, &mut term, 1.0);
    let after_open = rss_mb(pid);
    let mut worst = 0.0f64;
    for (i, c) in ('a'..='j').enumerate() {
        let t = Instant::now();
        unsafe { libc::write(std::os::fd::AsRawFd::as_raw_fd(&fd), [c as u8].as_ptr().cast(), 1) };
        let typed: String = ('a'..='j').take(i + 1).collect();
        until(&fd, &mut term, &typed, 5.0).expect("a typed key never appeared");
        worst = worst.max(t.elapsed().as_secs_f64());
    }
    let mut scroll = 0.0f64;
    for _ in 0..5 {
        let before = term.screen().contents();
        let t = Instant::now();
        unsafe { libc::write(std::os::fd::AsRawFd::as_raw_fd(&fd), b"\x1b[5~".as_ptr().cast(), 4) };
        while term.screen().contents() == before && t.elapsed().as_secs_f64() < 5.0 {
            pump(&fd, &mut term, 0.02);
        }
        scroll = scroll.max(t.elapsed().as_secs_f64());
    }
    let after_use = rss_mb(pid);
    eprintln!("BIG turns={turns} file={:.1}MB open={open:.2}s rss_open={after_open:.0}MB rss_after={after_use:.0}MB key_worst={:.0}ms pageup_worst={:.0}ms", body.len() as f64 / 1e6, worst * 1e3, scroll * 1e3);
    // Measured at 1500 turns, alone: open 0.9 s, 14 MB, keys and PageUp 38 ms. Beside the other pty tests on a busy machine
    // a latency reaches 0.8 s now and then, so the limits are far above that and far below what a redraw that grows with the
    // session would cost (seconds): a regression worth stopping on, not a slow minute.
    assert!(open < 8.0 && after_use < 150.0 && worst < 2.0 && scroll < 2.0, "slow or large on a long session");
    unsafe { libc::kill(pid, libc::SIGTERM) };
    pump(&fd, &mut term, 1.5);
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

/// The `mem 0M+14M` of the status line: what the window itself holds, in MB.
fn window_mb(parser: &vt100::Parser<Quiet>) -> Option<u64> {
    let screen = parser.screen().contents();
    let rest = screen.split("mem ").nth(1)?;
    rest.split_once("M+")?.1.split('M').next()?.parse().ok()
}

/// What the window says it holds comes from the supervisor, shared or private.
fn the_status_line_shows_what_the_supervisor_measured(tag: &str, env: &[(&str, &str)], shared: bool) {
    let home = session_home(tag);
    let (fd, pid) = spawn_with(&home, &["-c"], env);
    let _reap = Reap(pid, Some(home.clone()));
    let mut term = emulator();
    let t = Instant::now();
    while window_mb(&term).is_none_or(|mb| mb == 0) {
        assert!(t.elapsed() < Duration::from_secs(20), "no measured memory on the status line:\n{}", term.screen().contents());
        pump(&fd, &mut term, 0.2);
    }
    assert_eq!(home.join("sup/sup.sock").exists(), shared, "{}", if shared { "the shared supervisor serves this window" } else { "a private supervisor leaves no socket behind" });
    unsafe { libc::kill(pid, libc::SIGTERM) };
    pump(&fd, &mut term, 1.5);
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

#[test]
fn the_shared_supervisor_measures_the_window_and_the_status_line_shows_it() {
    the_status_line_shows_what_the_supervisor_measured("memsh", &[], true);
}

#[test]
fn a_window_without_the_shared_supervisor_is_measured_by_its_own() {
    the_status_line_shows_what_the_supervisor_measured("mempr", &[("CLAUDEPP_SUPERVISOR", "off")], false);
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A stand-in for `claude -p`: records its pid and waits for input until its stdin closes.
fn fake_claude(home: &std::path::Path) -> std::path::PathBuf {
    let script = home.join("fake-claude");
    std::fs::write(&script, format!("#!/bin/sh\necho $$ >> {}/pids\nwhile read l; do :; done\n", home.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    script
}

fn pids(home: &std::path::Path) -> Vec<i32> {
    std::fs::read_to_string(home.join("pids")).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
}

#[test]
fn over_the_budget_the_supervisor_has_the_window_stop_a_background_tab_and_the_tab_in_front_stays() {
    let home = session_home("bud");
    let fake = fake_claude(&home);
    // 1 MB: the window alone holds more, so something must go.
    let (fd, pid) = spawn_with(&home, &["-c", "--claude-bin", fake.to_str().unwrap(), "--mem-budget", "1"], &[]);
    let _reap = Reap(pid, Some(home.clone()));
    let _kill_fakes = KillAll(home.clone());
    let mut term = emulator();
    // Keys sent before the first frame are lost (the terminal is not raw yet): wait for the prompt, not for a time.
    until(&fd, &mut term, "❯", 30.0).expect("the window never drew its prompt");
    // Typing starts the tab's process (it is ready when you send); nothing has been sent, so it may be stopped.
    unsafe { libc::write(std::os::fd::AsRawFd::as_raw_fd(&fd), b"x".as_ptr().cast(), 1) };
    let t = Instant::now();
    while pids(&home).is_empty() {
        assert!(t.elapsed() < Duration::from_secs(20), "the first tab never started its process:\n{}", term.screen().contents());
        pump(&fd, &mut term, 0.2);
    }
    let first = pids(&home)[0];
    assert!(alive(first));
    // A second tab: the first is now in the background and may be stopped.
    unsafe { libc::write(std::os::fd::AsRawFd::as_raw_fd(&fd), b"\x15/new\r".as_ptr().cast(), 6) }; // ctrl+u clears the x
    let t = Instant::now();
    while alive(first) {
        assert!(t.elapsed() < Duration::from_secs(30), "the background tab was never stopped:\n{}", term.screen().contents());
        pump(&fd, &mut term, 0.2);
    }
    // The tab in front gets a process of its own as you type, and keeps it through more budget checks (every 6 s).
    unsafe { libc::write(std::os::fd::AsRawFd::as_raw_fd(&fd), b"y".as_ptr().cast(), 1) };
    let t = Instant::now();
    while pids(&home).len() < 2 {
        assert!(t.elapsed() < Duration::from_secs(20), "the second tab never started its process");
        pump(&fd, &mut term, 0.2);
    }
    pump(&fd, &mut term, 8.0);
    assert!(alive(pids(&home)[1]), "the tab in front keeps its process");
    unsafe { libc::kill(pid, libc::SIGTERM) };
    pump(&fd, &mut term, 1.5);
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

/// Kills the stand-in `claude` processes a test started, whatever happens to it.
struct KillAll(std::path::PathBuf);
impl Drop for KillAll {
    fn drop(&mut self) {
        for p in pids(&self.0) {
            // SAFETY: a signal to a process this test started.
            unsafe { libc::kill(p, libc::SIGKILL) };
        }
    }
}

/// What the supervisor says its settings are, asked over its socket.
fn supervisor_settings(home: &std::path::Path) -> Option<serde_json::Value> {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(home.join("sup/sup.sock")).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    writeln!(s, r#"{{"t":"hello","v":1,"pid":{}}}"#, std::process::id()).ok()?;
    writeln!(s, r#"{{"t":"status"}}"#).ok()?;
    for line in std::io::BufReader::new(s).lines() {
        let v: serde_json::Value = serde_json::from_str(&line.ok()?).ok()?;
        if v["t"] == "status" {
            return Some(v["settings"].clone());
        }
    }
    None
}

#[test]
fn the_supervisor_takes_the_saved_budget_and_idle_limit_and_a_flag_changes_them_for_everyone() {
    let home = session_home("cfg");
    std::fs::create_dir_all(home.join("claude++")).unwrap();
    std::fs::write(home.join("claude++/config.json"), r#"{"mem_budget_mb":"77","idle_suspend_s":"45"}"#).unwrap();
    let (fd, pid) = spawn(&home, &[]);
    let _reap = Reap(pid, Some(home.clone()));
    let mut term = emulator();
    let t = Instant::now();
    let settings = loop {
        pump(&fd, &mut term, 0.2);
        if let Some(s) = supervisor_settings(&home) {
            break s;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "no supervisor answered");
    };
    assert_eq!((settings["budget_mb"].clone(), settings["idle_s"].clone()), (serde_json::json!(77), serde_json::json!(45)), "what /config saved");
    // A second window with flags sets the value the supervisor holds, for every window.
    let (fd2, pid2) = spawn_with(&home, &["--mem-budget", "5000", "--idle-suspend", "90"], &[]);
    let _reap2 = Reap(pid2, None);
    let mut term2 = emulator();
    let t = Instant::now();
    loop {
        pump(&fd2, &mut term2, 0.2);
        let s = supervisor_settings(&home).unwrap();
        if s["budget_mb"] == 5000 && s["idle_s"] == 90 {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "the flags never reached the supervisor: {s}");
    }
    for (fd, pid, term) in [(&fd2, pid2, &mut term2), (&fd, pid, &mut term)] {
        unsafe { libc::kill(pid, libc::SIGTERM) };
        pump(fd, term, 1.5);
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
}
