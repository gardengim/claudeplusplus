//! One stdio MCP server process for several tabs (setting `share_mcp`).
//!
//! Claude Code starts its own copy of every MCP server for every conversation: three tabs in a project with
//! codegraph, caveman and entropy run nine server processes (codegraph alone is four). With sharing on, a tab
//! is given those servers as SDK-hosted ones instead (`initialize.sdkMcpServers`): its `claude` sends each MCP
//! message to us as an `mcp_message` control request, and we pass it on to one child process that every tab
//! with the same server, arguments, environment and folder uses. The model sees the same `mcp__<name>__<tool>`
//! tools.
//!
//! What this does for the tabs: each request id is remapped, so two tabs can both have a "request 1" in flight;
//! the handshake happens once (a later tab is answered with the first one's reply); a cancel is forwarded under
//! the remapped id; a server that exits is started again by the next message and shaken hands with again,
//! without the tab noticing. The process lives while any tab holds it and is stopped with the last one.
//!
//! What it does not do: route a server's own messages to a tab. Notifications (progress, `list_changed`,
//! logging) are dropped, and a request the server makes of its client (sampling, roots, elicitation) is refused.
//! Servers that depend on those, or keep state per conversation, should not be shared.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::thread;

/// How to start one server, and where: two tabs share a process only when all of this is equal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Def {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The folder it runs in (Claude Code starts a server in the conversation's folder).
    pub cwd: String,
}

impl Def {
    fn key(&self) -> String {
        json!([self.command, self.args, self.env, self.cwd]).to_string()
    }
}

/// Where a JSON-RPC reply goes: back to the tab that asked.
pub type Reply = Box<dyn FnOnce(Value) + Send>;

/// Give up on a server that keeps dying: this many starts without one finished handshake.
const MAX_STARTS: u32 = 3;

struct Queued {
    client: u64,
    msg: Value,
    reply: Reply,
}

enum Pending {
    Call { client: u64, id: Value, reply: Reply },
    Handshake,
}

enum Init {
    /// Nothing sent yet (or the process was just started again).
    Fresh,
    /// `initialize` is out; these wait for its reply.
    Waiting(Vec<Queued>),
    /// The server's `initialize` result, given to every tab after the first.
    Done(Value),
}

struct Inner {
    /// Lines for the writer thread; `None` while the process is not running.
    tx: Option<Sender<Vec<u8>>>,
    child: Option<std::process::Child>,
    /// Which start the running process is, so a stale reader thread cannot touch a newer one.
    epoch: u64,
    next: u64,
    pending: HashMap<u64, Pending>,
    init: Init,
    /// The first tab's `initialize` params, used again when the process has to be restarted.
    init_params: Option<Value>,
    starts: u32,
}

pub struct Server {
    def: Def,
    inner: Mutex<Inner>,
}

static REGISTRY: LazyLock<Mutex<HashMap<String, Weak<Server>>>> = LazyLock::new(Default::default);

/// The shared server for `def`: the running one, or a new one. The process stops when the last `Arc` is dropped.
pub fn acquire(def: &Def) -> Result<Arc<Server>, String> {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    reg.retain(|_, w| w.strong_count() > 0);
    let key = def.key();
    if let Some(s) = reg.get(&key).and_then(Weak::upgrade) && s.usable() {
        return Ok(s);
    }
    let s = Arc::new(Server {
        def: def.clone(),
        inner: Mutex::new(Inner { tx: None, child: None, epoch: 0, next: 0, pending: HashMap::new(), init: Init::Fresh, init_params: None, starts: 0 }),
    });
    {
        let mut i = s.lock();
        s.start(&mut i)?;
    }
    reg.insert(key, Arc::downgrade(&s));
    Ok(s)
}

/// How many shared server processes are running now.
pub fn live() -> usize {
    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    reg.values().filter_map(Weak::upgrade).filter(|s| s.lock().tx.is_some()).count()
}

fn error(id: &Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}})
}

impl Server {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn usable(&self) -> bool {
        let i = self.lock();
        i.tx.is_some() || i.starts < MAX_STARTS
    }

    /// Start the process. The caller holds the lock.
    fn start(self: &Arc<Self>, i: &mut Inner) -> Result<(), String> {
        let d = &self.def;
        let mut cmd = Command::new(&d.command);
        cmd.args(&d.args).envs(d.env.iter().cloned()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if std::path::Path::new(&d.cwd).is_dir() {
            cmd.current_dir(&d.cwd);
        }
        cmd.process_group(0); // its own group, so a server that has helper processes can be stopped whole
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", d.command))?;
        let (stdout, mut stdin) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for line in rx {
                if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        i.epoch += 1;
        i.starts += 1;
        i.tx = Some(tx);
        i.child = Some(child);
        i.init = Init::Fresh;
        let (weak, epoch) = (Arc::downgrade(self), i.epoch);
        thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
                let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
                let Some(me) = weak.upgrade() else { return };
                me.on_line(epoch, v);
            }
            if let Some(me) = weak.upgrade() {
                me.on_exit(epoch);
            }
        });
        Ok(())
    }

    fn send(i: &Inner, m: &Value) {
        if let (Some(tx), Ok(mut line)) = (&i.tx, serde_json::to_vec(m)) {
            line.push(b'\n');
            let _ = tx.send(line);
        }
    }

    /// One message from a tab. `reply` gets the JSON-RPC answer (for a notification, an empty result).
    pub fn route(self: &Arc<Self>, client: u64, msg: Value, reply: Reply) {
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let mut i = self.lock();
        let Some(id) = msg.get("id").cloned() else {
            // A notification. Only a cancel means anything to the server, and under our id for the request.
            if method == "notifications/cancelled" {
                let theirs = &msg["params"]["requestId"];
                let ours = i.pending.iter().find_map(|(k, p)| matches!(p, Pending::Call { client: c, id, .. } if *c == client && id == theirs).then_some(*k));
                if let Some(k) = ours {
                    Self::send(&i, &json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": k, "reason": msg["params"]["reason"]}}));
                }
            }
            drop(i);
            return reply(json!({"jsonrpc": "2.0", "result": {}}));
        };
        match &mut i.init {
            Init::Done(result) if method == "initialize" => {
                let r = json!({"jsonrpc": "2.0", "id": id, "result": result});
                drop(i);
                reply(r)
            }
            Init::Done(_) => self.forward(i, client, id, msg, reply),
            Init::Waiting(queue) => queue.push(Queued { client, msg, reply }),
            Init::Fresh => {
                if i.tx.is_none() {
                    if i.starts >= MAX_STARTS {
                        drop(i);
                        return reply(error(&id, &format!("{} keeps exiting; not started again", self.def.command)));
                    }
                    if let Err(e) = self.start(&mut i) {
                        drop(i);
                        return reply(error(&id, &e));
                    }
                }
                if method == "initialize" {
                    i.init_params = Some(msg["params"].clone());
                }
                let params = i.init_params.clone().unwrap_or_else(|| {
                    json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude++", "version": env!("CARGO_PKG_VERSION")}})
                });
                i.init = Init::Waiting(vec![Queued { client, msg, reply }]);
                i.next += 1;
                let ours = i.next;
                i.pending.insert(ours, Pending::Handshake);
                Self::send(&i, &json!({"jsonrpc": "2.0", "id": ours, "method": "initialize", "params": params}));
            }
        }
    }

    fn forward(&self, mut i: MutexGuard<'_, Inner>, client: u64, id: Value, mut msg: Value, reply: Reply) {
        if i.tx.is_none() {
            drop(i);
            return reply(error(&id, &format!("{} exited", self.def.command)));
        }
        i.next += 1;
        let ours = i.next;
        i.pending.insert(ours, Pending::Call { client, id, reply });
        msg["id"] = json!(ours);
        Self::send(&i, &msg);
    }

    /// Something the server wrote.
    fn on_line(self: &Arc<Self>, epoch: u64, v: Value) {
        if v.get("method").is_some() {
            // The server asking something of its client, or telling it something. There is no one tab to ask.
            if let Some(id) = v.get("id") {
                let i = self.lock();
                if i.epoch == epoch {
                    Self::send(&i, &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not available through claude++ shared servers"}}));
                }
            }
            return;
        }
        let Some(ours) = v["id"].as_u64() else { return };
        let mut i = self.lock();
        if i.epoch != epoch {
            return;
        }
        match i.pending.remove(&ours) {
            Some(Pending::Call { id, reply, .. }) => {
                drop(i);
                let mut v = v;
                v["id"] = id;
                reply(v);
            }
            Some(Pending::Handshake) => {
                let queue = match std::mem::replace(&mut i.init, Init::Fresh) {
                    Init::Waiting(q) => q,
                    _ => vec![],
                };
                let failed = v.get("result").is_none();
                if !failed {
                    i.init = Init::Done(v["result"].clone());
                    i.starts = 0;
                    Self::send(&i, &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
                }
                drop(i);
                for q in queue {
                    if failed {
                        let id = q.msg.get("id").cloned().unwrap_or(Value::Null);
                        (q.reply)(error(&id, &format!("{} refused to start: {}", self.def.command, v["error"]["message"].as_str().unwrap_or("no answer"))));
                    } else {
                        self.route(q.client, q.msg, q.reply);
                    }
                }
            }
            None => {}
        }
    }

    /// The process ended: everything waiting on it gets an error, and the next message starts it again.
    fn on_exit(self: &Arc<Self>, epoch: u64) {
        let mut i = self.lock();
        if i.epoch != epoch {
            return;
        }
        i.tx = None;
        let child = i.child.take();
        let waiting = match std::mem::replace(&mut i.init, Init::Fresh) {
            Init::Waiting(q) => q,
            _ => vec![],
        };
        let pending: Vec<Pending> = i.pending.drain().map(|(_, p)| p).collect();
        drop(i);
        if let Some(c) = child {
            crate::backend::reap(c, true);
        }
        let msg = format!("{} exited", self.def.command);
        for p in pending {
            if let Pending::Call { id, reply, .. } = p {
                reply(error(&id, &msg));
            }
        }
        for q in waiting {
            (q.reply)(error(&q.msg.get("id").cloned().unwrap_or(Value::Null), &msg));
        }
    }
}

impl Drop for Server {
    /// The last tab using it is gone: close its input so it can finish, and stop it if it lingers.
    fn drop(&mut self) {
        let i = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        i.tx = None;
        if let Some(c) = i.child.take() {
            crate::backend::reap(c, true);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// A server in sh: it logs each start to `log`, answers `initialize`, `tools/list` and `tools/call` (with its
    /// pid) and exits on `tools/call` for `die`. Ids are read out of the line.
    pub(crate) fn server(tag: &str) -> (Def, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-share-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let script = dir.join("srv.sh");
        std::fs::write(
            &script,
            r#"echo "start $$" >> "$LOG"
while IFS= read -r line; do
  id=$(printf %s "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && { echo "note $line" >> "$LOG"; continue; }
  case "$line" in
    *'"method":"initialize"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"t","version":"1"}}}';;
    *hang*) ;;
    *'"method":"tools/list"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"tools":[]}}';;
    *'die'*) exit 3;;
    *) echo '{"jsonrpc":"2.0","id":'$id',"result":{"pid":'$$'}}';;
  esac
done"#,
        )
        .unwrap();
        let def = Def { command: "/bin/sh".into(), args: vec![script.to_string_lossy().into()], env: vec![("LOG".into(), log.to_string_lossy().into())], cwd: dir.to_string_lossy().into() };
        (def, log)
    }

    fn ask(s: &Arc<Server>, client: u64, msg: Value) -> Value {
        let (tx, rx) = channel();
        s.route(client, msg, Box::new(move |v| tx.send(v).unwrap()));
        rx.recv_timeout(Duration::from_secs(5)).expect("a reply")
    }

    fn init(id: u64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}})
    }

    fn starts(log: &std::path::Path) -> usize {
        std::fs::read_to_string(log).unwrap_or_default().lines().filter(|l| l.starts_with("start")).count()
    }

    fn wait_starts(log: &std::path::Path, n: usize) {
        for _ in 0..100 {
            if starts(log) == n {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(starts(log), n);
    }

    #[test]
    fn tabs_with_the_same_server_share_one_process_and_keep_their_own_ids() {
        let (def, log) = server("two");
        let a = acquire(&def).unwrap();
        let b = acquire(&def).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same definition, same server");
        let other = acquire(&Def { cwd: "/".into(), ..def.clone() }).unwrap();
        assert!(!Arc::ptr_eq(&a, &other), "another folder is another process");
        // Both tabs call their first request "1": each gets its own answer back under its own id.
        let ra = ask(&a, 1, init(1));
        let rb = ask(&b, 2, init(1));
        assert_eq!(ra["id"], 1);
        assert_eq!(rb["id"], 1);
        assert_eq!(ra["result"], rb["result"], "the second tab is answered from the first handshake");
        wait_starts(&log, 2); // one process for the two tabs, one for the other folder (it logs to the same file)
        let p1 = ask(&a, 1, json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "x"}}));
        let p2 = ask(&b, 2, json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "x"}}));
        assert_eq!((&p1["id"], &p2["id"]), (&json!(7), &json!(7)));
        assert_eq!(p1["result"]["pid"], p2["result"]["pid"], "the same process answered both");
        // The server saw `initialized` once, not once per tab.
        std::thread::sleep(Duration::from_millis(200));
        let notes = std::fs::read_to_string(&log).unwrap().lines().filter(|l| l.contains("initialized")).count();
        assert_eq!(notes, 1, "`initialized` once for the process, not once per tab");
        assert!(live() >= 2);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn a_cancel_reaches_the_server_under_its_own_id_for_the_request() {
        let (def, log) = server("cancel");
        let s = acquire(&def).unwrap();
        ask(&s, 1, init(1));
        // Two tabs each have a request 5 in flight that the server never answers; tab 2 cancels its own.
        let (tx, rx) = channel();
        s.route(1, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "hang"}}), Box::new(move |v| tx.send(v).unwrap()));
        s.route(2, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "hang"}}), Box::new(|_| {}));
        let ack = ask(&s, 2, json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5, "reason": "stop"}}));
        assert!(ack["result"].is_object(), "a notification is acknowledged");
        for _ in 0..100 {
            if std::fs::read_to_string(&log).unwrap().contains("cancelled") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let text = std::fs::read_to_string(&log).unwrap();
        let note = text.lines().find(|l| l.contains("cancelled")).unwrap_or_default();
        assert!(note.contains(r#""requestId":3"#), "tab 2's request was our id 3 (init was 1, tab 1's call 2): {note}");
        assert!(rx.try_recv().is_err(), "tab 1's request is still waiting");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn a_process_that_exits_is_started_again_and_shaken_hands_with_unseen() {
        let (def, log) = server("die");
        let s = acquire(&def).unwrap();
        ask(&s, 1, init(1));
        let r = ask(&s, 1, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "die"}}));
        assert_eq!(r["id"], 2);
        assert!(r["error"]["message"].as_str().unwrap().contains("exited"), "a call in flight gets an error: {r}");
        // The tab does not initialize again; the next message restarts the server and does it for the tab.
        let r = ask(&s, 1, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}));
        assert_eq!(r["id"], 3);
        assert!(r["result"]["tools"].is_array(), "{r}");
        wait_starts(&log, 2);
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }

    #[test]
    fn a_server_that_cannot_start_is_an_error_and_the_last_tab_leaving_stops_the_process() {
        assert!(acquire(&Def { command: "/nonexistent/claudepp-mcp".into(), ..Default::default() }).is_err());
        let (def, log) = server("drop");
        let s = acquire(&def).unwrap();
        ask(&s, 1, init(1));
        let pid: i32 = std::fs::read_to_string(&log).unwrap().lines().next().unwrap().trim_start_matches("start ").parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "running while held");
        drop(s);
        crate::backend::wait_for_exits();
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "gone with the last holder");
        let _ = std::fs::remove_dir_all(log.parent().unwrap());
    }
}
