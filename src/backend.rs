//! Headless Claude Code process: `claude -p` speaking stream-json over stdin/stdout.
//! One process per session; it stays alive across turns while stdin is open.

use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

/// What a backend reader thread reports, tagged with the owning session id.
pub enum Msg {
    Line(Value),
    Exited { status: String, stderr_tail: String },
}

pub struct Backend {
    child: Child,
    stdin: Option<ChildStdin>,
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
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let v = serde_json::from_str(&line)
                    .unwrap_or_else(|_| json!({"type": "_raw", "text": line}));
                if tx.send(wrap(pid, Msg::Line(v))).is_err() {
                    return;
                }
            }
            let stderr_tail = err_thread.join().unwrap_or_default();
            let _ = tx.send(wrap(pid, Msg::Exited { status: "exited".into(), stderr_tail }));
        });

        let stdin = child.stdin.take();
        let mut b = Backend { child, stdin, pid, next_id: 0 };
        b.control("initialize", json!({}))?;
        Ok(b)
    }

    /// Signal EOF; claude finishes the current write and exits. Drop then waits briefly.
    pub fn close(&mut self) {
        self.stdin = None;
    }

    fn send(&mut self, v: &Value) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "backend stdin closed"))?;
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        stdin.write_all(&line)?;
        stdin.flush()
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

impl Drop for Backend {
    /// Close stdin so claude can finish writing its transcript and exit on its own;
    /// kill it only if it lingers.
    fn drop(&mut self) {
        self.stdin = None;
        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    fn lingering_process_is_killed_on_drop() {
        let launch = fake("hang", "trap '' TERM; while :; do sleep 1; done");
        let (tx, _rx) = mpsc::channel();
        let b = Backend::spawn(&launch, Resume::New, tx, |_, m| m).unwrap();
        let t = Instant::now();
        drop(b);
        assert!(t.elapsed() < Duration::from_secs(3));
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
