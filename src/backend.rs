//! Headless Claude Code process: `claude -p` speaking stream-json over stdin/stdout.
//! One process per session; it stays alive across turns while stdin is open.

use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// What a backend reader thread reports, tagged with the owning session id.
pub enum Msg {
    Line(Value),
    Exited { status: String, stderr_tail: String },
}

pub struct Backend {
    child: Option<Child>,
    /// Lines for the writer thread; dropping it closes claude's stdin once the queue is written.
    stdin: Option<Sender<Vec<u8>>>,
    pub pid: u32,
    next_id: u64,
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
                match out.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.trim().is_empty() {
                    continue;
                }
                let v = serde_json::from_str(line)
                    .unwrap_or_else(|_| json!({"type": "_raw", "text": line}));
                if tx.send(wrap(pid, Msg::Line(v))).is_err() {
                    return;
                }
            }
            let stderr_tail = err_thread.join().unwrap_or_default();
            let _ = tx.send(wrap(pid, Msg::Exited { status: "exited".into(), stderr_tail }));
        });

        // Writes go through a thread: a multi-megabyte image message, or a child that stops
        // reading, must not block the UI on a full pipe.
        let mut pipe = child.stdin.take().unwrap();
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for line in stdin_rx {
                if pipe.write_all(&line).is_err() {
                    break;
                }
            }
        });
        let stdin = Some(stdin_tx);
        let mut b = Backend { child: Some(child), stdin, pid, next_id: 0 };
        b.control("initialize", json!({}))?;
        Ok(b)
    }

    /// Signal EOF; claude finishes the current write and exits. Drop then waits briefly.
    pub fn close(&mut self) {
        self.stdin = None;
    }

    fn send(&mut self, v: &Value) -> io::Result<()> {
        let closed = || io::Error::new(io::ErrorKind::BrokenPipe, "backend stdin closed");
        let stdin = self.stdin.as_ref().ok_or_else(closed)?;
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        stdin.send(line).map_err(|_| closed())
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
        self.stdin = None;
        let Some(mut child) = self.child.take() else { return };
        let reaper = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(800);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
        if let Ok(mut r) = REAPERS.lock() {
            r.retain(|h| !h.is_finished());
            r.push(reaper);
        }
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
}
