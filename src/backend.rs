//! Headless Claude Code process: `claude -p` speaking stream-json over stdin/stdout.
//! One process per session; it stays alive across turns while stdin is open.

use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What a backend reader thread reports, tagged with the owning session id.
pub enum Msg {
    Line(Value),
    Exited { status: String, stderr_tail: String },
}

pub struct Backend {
    child: Option<Child>,
    /// Lines for claude's stdin.
    pipe: Arc<Pipe>,
    pub pid: u32,
    next_id: u64,
}

/// Claude's stdin, as lines for a writer thread. Shared with the reader thread, which answers MCP messages for
/// the servers we host; `close` ends it for everyone, so claude still sees end-of-input when the tab lets go.
struct Pipe(Mutex<Option<Sender<Vec<u8>>>>);

impl Pipe {
    fn send(&self, mut line: Vec<u8>) -> bool {
        line.push(b'\n');
        self.0.lock().is_ok_and(|g| g.as_ref().is_some_and(|tx| tx.send(line).is_ok()))
    }

    fn close(&self) {
        if let Ok(mut g) = self.0.lock() {
            *g = None;
        }
    }
}

/// How to launch `claude`. `args` already contains the optimization profile and passthrough args.
#[derive(Clone, Default)]
pub struct Launch {
    pub bin: String,
    pub args: Vec<String>,
    /// Extra environment for the child, e.g. Bun/JSC runtime knobs.
    pub env: Vec<(String, String)>,
    /// Working directory of the child; `None` inherits ours.
    pub cwd: Option<String>,
    /// MCP servers this process gets from claude++ instead of starting its own (see `share`): name and definition.
    pub shared: Vec<(String, crate::share::Def)>,
    /// Terminals this tab's Claude can drive, hosted here as the MCP server `term` (see `term`).
    pub terms: Option<Arc<crate::term::Terms>>,
}

/// A count read from the protocol (tokens, milliseconds, sizes). It comes from another process, so it is capped
/// at 2^40: far beyond anything real, and sums of many of them cannot overflow.
pub fn count(v: &serde_json::Value) -> u64 {
    v.as_u64().unwrap_or(0).min(1 << 40)
}

pub enum Resume<'a> {
    New,
    Id(&'a str),
    Continue,
}

impl Backend {
    pub fn spawn<E: Send + 'static>(
        launch: &Launch,
        resume: Resume,
        tx: Sender<E>,
        wrap: impl Fn(u32, Msg) -> E + Send + 'static,
    ) -> io::Result<Self> {
        // Started first so they come up while claude does. One that cannot start still answers, with that error.
        let shares: HashMap<String, Result<Arc<crate::share::Server>, String>> = launch.shared.iter().map(|(n, d)| (n.clone(), crate::share::acquire(d))).collect();
        let terms = launch.terms.clone();
        let mut cmd = Command::new(&launch.bin);
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
        ]);
        cmd.args(&launch.args).envs(launch.env.iter().cloned());
        if let Some(dir) = &launch.cwd {
            cmd.current_dir(dir);
        }
        match resume {
            Resume::New => {}
            Resume::Id(id) => {
                cmd.args(["--resume", id]);
            }
            Resume::Continue => {
                cmd.arg("--continue");
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        // Writes go through a thread: a multi-megabyte image message, or a child that stops
        // reading, must not block the UI on a full pipe.
        let mut stdin_pipe = child.stdin.take().unwrap();
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for line in stdin_rx {
                if stdin_pipe.write_all(&line).is_err() {
                    break;
                }
            }
        });
        let pipe = Arc::new(Pipe(Mutex::new(Some(stdin_tx))));
        let client = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        let reader_pipe = pipe.clone();

        // stderr is only interesting when the process dies; keep the tail.
        let err_thread = thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            let lines: Vec<&str> = buf.lines().collect();
            lines[lines.len().saturating_sub(15)..].join("\n")
        });
        thread::spawn(move || {
            // Bytes, not `lines()`: one non-UTF-8 line must not stop the reader (and stall the child).
            let mut out = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                if buf.capacity() > 1 << 20 {
                    buf = Vec::new(); // one huge line must not keep its tens of megabytes for the life of the tab
                }
                match out.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                let mut v = serde_json::from_str(line)
                    .unwrap_or_else(|_| json!({"type": "_raw", "text": line}));
                if v["type"] == "control_request" && v["request"]["subtype"] == "mcp_message" {
                    answer_mcp(&v, client, &shares, &terms, &reader_pipe);
                    continue;
                }
                crate::image::pre_convert(&mut v);
                if tx.send(wrap(pid, Msg::Line(v))).is_err() {
                    return;
                }
            }
            let stderr_tail = err_thread.join().unwrap_or_default();
            let _ = tx.send(wrap(pid, Msg::Exited { status: "exited".into(), stderr_tail }));
        });

        let mut b = Backend { child: Some(child), pipe, pid, next_id: 0 };
        // The servers we host are named once, here; a later `initialize` leaves them as they are.
        let mut hosted: Vec<&str> = launch.shared.iter().map(|(n, _)| n.as_str()).collect();
        if launch.terms.is_some() {
            hosted.push("term");
        }
        b.control("initialize", if hosted.is_empty() { json!({}) } else { json!({"sdkMcpServers": hosted}) })?;
        Ok(b)
    }

    /// Signal EOF; claude finishes the current write and exits. Drop then waits briefly.
    pub fn close(&mut self) {
        self.pipe.close();
    }

    fn send(&mut self, v: &Value) -> io::Result<()> {
        if self.pipe.send(serde_json::to_vec(v)?) { Ok(()) } else { Err(io::Error::new(io::ErrorKind::BrokenPipe, "backend stdin closed")) }
    }

    /// `content` is a string or an array of content blocks (text/image).
    pub fn user(&mut self, content: Value) -> io::Result<()> {
        self.send(&json!({
            "type": "user",
            "session_id": "",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": content},
        }))
    }

    pub fn control(&mut self, subtype: &str, mut fields: Value) -> io::Result<()> {
        self.next_id += 1;
        fields["subtype"] = json!(subtype);
        self.send(&json!({
            "type": "control_request",
            "request_id": format!("cpp-{}", self.next_id),
            "request": fields,
        }))
    }

    pub fn respond(&mut self, request_id: &str, response: Value) -> io::Result<()> {
        self.send(&json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response},
        }))
    }

    pub fn respond_error(&mut self, request_id: &str, error: &str) -> io::Result<()> {
        self.send(&json!({
            "type": "control_response",
            "response": {"subtype": "error", "request_id": request_id, "error": error},
        }))
    }
}

/// Threads still waiting for a dropped backend to exit; `wait_for_exits` joins them at quit.
static REAPERS: Mutex<Vec<thread::JoinHandle<()>>> = Mutex::new(Vec::new());

impl Drop for Backend {
    /// Close stdin so claude can finish writing its transcript and exit on its own;
    /// kill it only if it lingers. The wait runs off the UI thread (closing tabs or
    /// suspending several must not freeze it).
    fn drop(&mut self) {
        self.pipe.close();
        if let Some(child) = self.child.take() {
            reap(child, false);
        }
    }
}

/// Wait briefly for a child whose input is closed to exit by itself, then kill it (the whole process group
/// when it leads one: an MCP server with helper processes). Runs off the calling thread; `wait_for_exits` joins.
pub fn reap(mut child: Child, group: bool) {
    let reaper = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        if group {
            unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    });
    if let Ok(mut r) = REAPERS.lock() {
        r.retain(|h| !h.is_finished());
        r.push(reaper);
    }
}

static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);

/// Claude asked for an MCP server we host: pass the message to the shared process and send its answer back.
fn answer_mcp(v: &Value, client: u64, shares: &HashMap<String, Result<Arc<crate::share::Server>, String>>, terms: &Option<Arc<crate::term::Terms>>, pipe: &Arc<Pipe>) {
    let (rid, req) = (v["request_id"].clone(), &v["request"]);
    let msg = req["message"].clone();
    let pipe = pipe.clone();
    let reply: crate::share::Reply = Box::new(move |resp| {
        let line = json!({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": {"mcp_response": resp}}});
        if let Ok(b) = serde_json::to_vec(&line) {
            pipe.send(b);
        }
    });
    let fail = |e: &str| json!({"jsonrpc": "2.0", "id": msg.get("id").cloned().unwrap_or(Value::Null), "error": {"code": -32000, "message": e}});
    if let (Some(t), Some("term")) = (terms, req["server_name"].as_str()) {
        return t.handle(&msg, reply);
    }
    match req["server_name"].as_str().and_then(|n| shares.get(n)) {
        Some(Ok(server)) => server.route(client, msg.clone(), reply),
        Some(Err(e)) => reply(fail(e)),
        None => reply(fail("no such server")),
    }
}

/// Block until every dropped backend has exited or been killed (at quit, so none is left behind).
pub fn wait_for_exits() {
    let reapers = REAPERS.lock().map(|mut r| std::mem::take(&mut *r)).unwrap_or_default();
    for r in reapers {
        let _ = r.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn fake(name: &str, body: &str) -> Launch {
        let path = std::env::temp_dir().join(format!("claudeplusplus-fake-{name}-{}", std::process::id()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&path).status().unwrap();
        Launch { bin: path.to_string_lossy().into(), ..Default::default() }
    }

    #[test]
    fn round_trip_and_clean_exit_on_stdin_close() {
        let launch = fake(
            "ok",
            r#"echo '{"type":"system","subtype":"init","session_id":"s1"}'
while IFS= read -r line; do
  case "$line" in *'"type":"user"'*) echo '{"type":"result","result":"ok"}';; esac
done
echo bye >&2"#,
        );
        let (tx, rx) = mpsc::channel();
        let mut b = Backend::spawn(&launch, Resume::New, tx, |_, m| m).unwrap();
        let next = || match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Line(v) => v,
            Msg::Exited { .. } => panic!("exited early"),
        };
        assert_eq!(next()["subtype"], "init");
        b.user(json!("hi")).unwrap();
        assert_eq!(next()["type"], "result");
        drop(b);
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Exited { stderr_tail, .. } => assert_eq!(stderr_tail, "bye"),
            Msg::Line(v) => panic!("unexpected {v}"),
        }
    }

    #[test]
    fn a_big_message_to_a_child_that_is_not_reading_never_blocks_the_sender() {
        let launch = fake("deaf", "sleep 30");
        let (tx, _rx) = mpsc::channel();
        let mut b = Backend::spawn(&launch, Resume::New, tx, |_, m| m).unwrap();
        let t = Instant::now();
        b.user(json!("x".repeat(2_000_000))).unwrap(); // far more than a pipe buffer holds
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
    }

    #[test]
    fn lingering_process_is_killed_on_drop() {
        let launch = fake("hang", "trap '' TERM; while :; do sleep 1; done");
        let (tx, _rx) = mpsc::channel();
        let b = Backend::spawn(&launch, Resume::New, tx, |_, m| m).unwrap();
        let (t, pid) = (Instant::now(), b.pid.to_string());
        drop(b);
        assert!(t.elapsed() < Duration::from_millis(100), "dropping never waits on the caller's thread");
        wait_for_exits();
        assert!(t.elapsed() < Duration::from_secs(3));
        let alive = Command::new("kill").args(["-0", &pid]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
        assert!(!alive, "the lingering process was killed");
    }

    #[test]
    fn child_runs_in_the_launch_directory() {
        let mut launch = fake("cwd", r#"printf '{"type":"cwd","dir":"%s"}\n' "$(pwd -P)""#);
        launch.cwd = Some(std::fs::canonicalize(std::env::temp_dir()).unwrap().to_string_lossy().into());
        let (tx, rx) = mpsc::channel();
        let _b = Backend::spawn(&launch, Resume::New, tx, |_, m| m).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Line(v) => assert_eq!(v["dir"].as_str(), launch.cwd.as_deref()),
            Msg::Exited { .. } => panic!("exited early"),
        }
    }

    #[test]
    fn the_terminal_server_is_named_to_claude_and_answers_in_process_without_a_child() {
        let out = std::env::temp_dir().join(format!("claude-out-term-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let mut launch = fake(
            "term",
            r#"while IFS= read -r line; do
  case "$line" in
    *sdkMcpServers*)
      echo "$line" >> "$OUT"
      echo '{"type":"control_request","request_id":"t1","request":{"subtype":"mcp_message","server_name":"term","message":{"jsonrpc":"2.0","id":7,"method":"tools/list"}}}';;
    *'"type":"control_response"'*) echo "$line" >> "$OUT";;
  esac
done"#,
        );
        launch.env.push(("OUT".into(), out.to_string_lossy().into()));
        launch.terms = Some(crate::term::Terms::new(Arc::new(|| {})));
        let (tx, rx) = mpsc::channel();
        let b = Backend::spawn(&launch, Resume::New, tx, |_, m| m).unwrap();
        let text = (0..250)
            .find_map(|_| {
                let t = std::fs::read_to_string(&out).unwrap_or_default();
                (t.lines().count() >= 2).then_some(t).or_else(|| {
                    thread::sleep(Duration::from_millis(20));
                    None
                })
            })
            .expect("claude was told about the server and got its answer");
        assert!(text.contains(r#""sdkMcpServers":["term"]"#), "{text}");
        assert!(text.contains(r#""request_id":"t1""#) && text.contains("term_open") && text.contains("term_send") && text.contains(r#""id":7"#), "{text}");
        assert!(rx.try_recv().is_err(), "the UI never sees these messages");
        drop(b);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn two_backends_get_the_same_hosted_mcp_server_and_still_see_end_of_input() {
        let (def, log) = crate::share::tests::server("backend");
        let out = log.with_file_name("claude-out");
        // The fake claude names the server in `initialize`, then asks it two things over the pipe, like the real one.
        let mut launch = fake(
            "mcp",
            r#"while IFS= read -r line; do
  case "$line" in
    *sdkMcpServers*)
      echo "$line" >> "$OUT"
      echo '{"type":"control_request","request_id":"m1","request":{"subtype":"mcp_message","server_name":"srv","message":{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}}}'
      echo '{"type":"control_request","request_id":"m2","request":{"subtype":"mcp_message","server_name":"srv","message":{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{}}}}';;
    *'"type":"control_response"'*) echo "$line" >> "$OUT";;
  esac
done
echo bye >&2"#,
        );
        launch.env.push(("OUT".into(), out.to_string_lossy().into()));
        launch.shared = vec![("srv".into(), def)];
        let lines = |n: usize| {
            for _ in 0..250 {
                let t = std::fs::read_to_string(&out).unwrap_or_default();
                if t.lines().count() >= n {
                    return t;
                }
                thread::sleep(Duration::from_millis(20));
            }
            panic!("only {:?}", std::fs::read_to_string(&out));
        };
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        let a = Backend::spawn(&launch, Resume::New, tx1, |_, m| m).unwrap();
        let b = Backend::spawn(&launch, Resume::New, tx2, |_, m| m).unwrap();
        let text = lines(6); // per backend: the initialize line and two answers
        assert_eq!(text.matches(r#""sdkMcpServers":["srv"]"#).count(), 2, "{text}");
        let pids: Vec<&str> = text.lines().filter(|l| l.contains("mcp_response") && l.contains(r#""id":2"#)).map(|l| l.split(r#""pid":"#).nth(1).unwrap().split(['}', ',']).next().unwrap()).collect();
        assert_eq!(pids.len(), 2, "{text}");
        assert_eq!(pids[0], pids[1], "one server process answered both backends");
        assert!(text.contains(r#""request_id":"m1""#) && text.contains(r#""request_id":"m2""#));
        // The messages were ours to answer: the UI never sees them.
        assert!(rx1.try_recv().is_err() && rx2.try_recv().is_err());
        // Letting go still ends claude's input (the reader thread holds the pipe too), so it exits by itself.
        drop((a, b));
        for rx in [rx1, rx2] {
            match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                Msg::Exited { stderr_tail, .. } => assert_eq!(stderr_tail, "bye"),
                Msg::Line(v) => panic!("unexpected {v}"),
            }
        }
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }
}
