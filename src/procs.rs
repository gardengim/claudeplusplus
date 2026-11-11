//! Every Claude-related process on the machine, grouped by who it belongs to (`claude++ procs`).
//!
//! Read-only: nothing here signals a process. Only what descends from this claude++ is `Ours`; everything else is
//! shown and never controlled (the supervisor acts on processes claude++ itself spawned, and on no others).

use crate::panel::{self, View, cycle};
use crate::supervisor::Peer;
use crate::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// This claude++ and everything it spawned: backends, MCP servers, terminals.
    Ours,
    /// The supervisor (`claude++ supervisor`): one per user, whoever started it.
    Supervisor,
    /// Another claude++ instance and everything below it. Its own instance controls it.
    Peer,
    /// A `claude` process claude++ did not spawn (interactive, background, or a worker).
    Claude,
    /// Claude Code's supervisor (`claude daemon run`).
    Daemon,
    /// A pre-warmed worker of that daemon (`--bg-spare`): no conversation yet.
    Spare,
    /// The terminal host of a background session (`--bg-pty-host`).
    PtyHost,
    /// An MCP server whose parent is not one of ours.
    Mcp,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Ours => "ours (this claude++)",
            Kind::Supervisor => "claude++ supervisor",
            Kind::Peer => "another claude++",
            Kind::Claude => "claude (not ours)",
            Kind::Daemon => "claude daemon",
            Kind::Spare => "daemon spare workers",
            Kind::PtyHost => "background session hosts",
            Kind::Mcp => "MCP servers (not ours)",
            Kind::Other => "other",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Kind::Ours => "ours",
            Kind::Supervisor => "super",
            Kind::Peer => "peer",
            Kind::Claude => "claude",
            Kind::Daemon => "daemon",
            Kind::Spare => "spare",
            Kind::PtyHost => "host",
            Kind::Mcp => "mcp",
            Kind::Other => "other",
        }
    }
}

pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub cmd: String,
}

fn tok<'a>(s: &mut &'a str) -> Option<&'a str> {
    let t = s.trim_start();
    if t.is_empty() {
        return None;
    }
    let (a, b) = t.split_at(t.find(char::is_whitespace).unwrap_or(t.len()));
    *s = b;
    Some(a)
}

/// Lines of `ps -A -o pid=,ppid=,command=`.
pub fn parse_ps(out: &str) -> Vec<Proc> {
    out.lines()
        .filter_map(|l| {
            let mut s = l;
            let pid = tok(&mut s)?.parse().ok()?;
            let ppid = tok(&mut s)?.parse().ok()?;
            Some(Proc { pid, ppid, cmd: s.trim().to_string() })
        })
        .collect()
}

pub fn snapshot() -> Vec<Proc> {
    let out = Command::new("ps").args(["-A", "-o", "pid=,ppid=,command="]).output();
    parse_ps(&String::from_utf8_lossy(&out.map(|o| o.stdout).unwrap_or_default()))
}

fn exe(cmd: &str) -> &str {
    cmd.split_whitespace().next().unwrap_or("")
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_plusplus(cmd: &str) -> bool {
    matches!(base(exe(cmd)), "claudeplusplus" | "claude++")
}

fn is_supervisor(cmd: &str) -> bool {
    is_plusplus(cmd) && cmd.split_whitespace().nth(1) == Some("supervisor")
}

fn is_claude(cmd: &str) -> bool {
    let e = exe(cmd);
    base(e) == "claude" || e.contains("/claude/versions/")
}

/// Another app's own tools (the Codex app starts a codegraph of its own): not Claude's, whatever they are.
fn foreign_app(cmd: &str) -> bool {
    cmd.contains("Codex") || cmd.contains("ChatGPT")
}

/// Whole words only: `--mcp-oauth-redirect-url` (Docker's agent) is not an MCP server.
fn is_mcp(cmd: &str) -> bool {
    cmd.contains("codegraph") || cmd.split_whitespace().any(|t| t == "--mcp" || base(t).ends_with("-mcp"))
}

/// The kind of each process, in the order given. `me` is this claude++'s pid.
pub fn classify(procs: &[Proc], me: u32) -> Vec<Kind> {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    let ancestors = |p: &Proc| {
        let (mut v, mut cur) = (vec![], p.ppid);
        while v.len() < 32 {
            match by_pid.get(&cur) {
                Some(q) if q.pid != p.pid => {
                    v.push(*q);
                    cur = q.ppid;
                }
                _ => break,
            }
        }
        v
    };
    procs
        .iter()
        .map(|p| {
            let up = ancestors(p);
            // Its shared MCP servers (`share_mcp`) are its children and belong to it.
            if is_supervisor(&p.cmd) || up.iter().any(|a| is_supervisor(&a.cmd)) {
                return Kind::Supervisor;
            }
            if p.pid == me || up.iter().any(|a| a.pid == me) {
                return Kind::Ours;
            }
            if is_plusplus(&p.cmd) || up.iter().any(|a| is_plusplus(&a.cmd)) {
                return Kind::Peer;
            }
            let c = p.cmd.as_str();
            if c.contains("--bg-spare") {
                Kind::Spare
            } else if c.contains("--bg-pty-host") {
                Kind::PtyHost
            } else if c.contains("claude daemon run") {
                Kind::Daemon
            } else if is_claude(c) {
                Kind::Claude
            } else if is_mcp(c) && !foreign_app(c) && !up.iter().any(|a| foreign_app(&a.cmd)) {
                Kind::Mcp
            } else {
                Kind::Other
            }
        })
        .collect()
}

pub struct Row {
    pub pid: u32,
    pub kind: Kind,
    pub mb: f64,
    pub cmd: String,
}

pub struct Report {
    pub rows: Vec<Row>,
    /// `footprint`, or RSS (which overstates) where `footprint` is not available.
    pub source: &'static str,
}

const ORDER: [Kind; 8] = [Kind::Ours, Kind::Supervisor, Kind::Peer, Kind::Claude, Kind::Daemon, Kind::Spare, Kind::PtyHost, Kind::Mcp];

/// Read the process list and the memory of the Claude-related ones. Slow (tens of milliseconds, more with many
/// processes): call it off the UI thread. `None` when `ps` cannot run.
pub fn report() -> Option<Report> {
    let procs = snapshot();
    if procs.is_empty() {
        return None;
    }
    let kinds = classify(&procs, std::process::id());
    let mut rel: Vec<(&Proc, Kind)> = procs.iter().zip(kinds).filter(|(_, k)| *k != Kind::Other).collect();
    rel.sort_by_key(|(p, k)| (*k, p.pid));
    let pids: Vec<u32> = rel.iter().map(|(p, _)| p.pid).collect();
    let fp = crate::memory::footprint_kb(&pids);
    let (mem, source) = if fp.is_empty() { (crate::memory::rss_kb(&pids), "RSS, which overstates") } else { (fp, "footprint") };
    let mem: HashMap<u32, u64> = mem.into_iter().collect();
    let rows = rel.into_iter().map(|(p, kind)| Row { pid: p.pid, kind, mb: mem.get(&p.pid).copied().unwrap_or(0) as f64 / 1024.0, cmd: p.cmd.clone() }).collect();
    Some(Report { rows, source })
}

/// (kind, processes, MB) for each group, in display order; empty groups included.
pub fn totals(rows: &[Row]) -> Vec<(Kind, usize, f64)> {
    ORDER.iter().map(|&k| (k, rows.iter().filter(|r| r.kind == k).count(), rows.iter().filter(|r| r.kind == k).map(|r| r.mb).sum())).collect()
}

/// Spare workers below this much memory are not worth a line.
const HINT_MB: f64 = 200.0;

/// One line about Claude Code's own idle workers, when they hold real memory. They are not ours, so claude++
/// leaves them alone; this only says where they came from and where to look.
pub fn spare_hint(r: &Report) -> Option<String> {
    let mb: f64 = r.rows.iter().filter(|x| matches!(x.kind, Kind::Spare | Kind::Daemon)).map(|x| x.mb).sum();
    let workers = r.rows.iter().filter(|x| x.kind == Kind::Spare && x.cmd.split_whitespace().nth(1) == Some("bg-spare")).count();
    (workers > 0 && mb >= HINT_MB).then(|| {
        format!(
            "Claude Code's background daemon is holding {mb:.0} MB, {workers} idle spare worker{} that no conversation uses. They come from interactive `claude` windows (not from claude++), so they are left alone; /procs lists them.",
            if workers == 1 { "" } else { "s" }
        )
    })
}

/// `claude++ procs`: a table of the groups, then every process in them.
pub fn print() {
    let Some(r) = report() else { return eprintln!("claude++: could not read the process list (ps)") };
    println!("memory is {}; only \"ours\" is ever controlled by claude++\n", r.source);
    for (k, n, mb) in totals(&r.rows) {
        println!("{:<28} {:>3} processes {:>7.0} MB", k.label(), n, mb);
    }
    println!("{:<28} {:>3} processes {:>7.0} MB\n", "total", r.rows.len(), r.rows.iter().map(|x| x.mb).sum::<f64>());
    let mut last = None;
    for x in &r.rows {
        if last != Some(x.kind) {
            println!("{}", x.kind.label());
            last = Some(x.kind);
        }
        println!("  {:>6} {:>6.0} MB  {}", x.pid, x.mb, x.cmd.chars().take(100).collect::<String>());
    }
}

/// `/procs`: what is running, who it belongs to, and what it holds. Read-only; `r` reads it again.
#[derive(Default)]
pub struct Ui {
    /// `None` while the list is being read.
    pub report: Option<Report>,
    cursor: usize,
    /// `r` was pressed: the caller reads the list again.
    pub reload: bool,
}

impl Ui {
    /// `false` once the panel should close.
    pub fn key(&mut self, k: KeyEvent) -> bool {
        let ctrl_c = crate::panel::is_ctrl_c(&k);
        if ctrl_c || k.code == KeyCode::Esc {
            return false;
        }
        let n = self.report.as_ref().map_or(0, |r| r.rows.len());
        if n > 0 {
            self.cursor = self.cursor.min(n - 1);
            match k.code {
                KeyCode::Up => self.cursor = cycle(self.cursor, n, true),
                KeyCode::Down => self.cursor = cycle(self.cursor, n, false),
                _ => {}
            }
        }
        if k.code == KeyCode::Char('r') {
            self.reload = true;
        }
        true
    }

    pub fn set(&mut self, r: Option<Report>) {
        self.report = r;
        self.cursor = 0;
    }
}

pub fn view(ui: &Ui, peers: &[Peer], me: u32) -> View {
    let keys = theme::hints(&[("↑↓", "move"), ("r", "refresh"), ("esc", "close")]);
    let Some(r) = &ui.report else { return View::new("Processes", vec![Line::styled("reading the process list…", theme::dim())], 0, keys) };
    if r.rows.is_empty() {
        return View::new("Processes", vec![Line::styled("no Claude-related processes found", theme::dim())], 0, keys);
    }
    let mut head: Vec<Line<'static>> = totals(&r.rows)
        .into_iter()
        .filter(|t| t.1 > 0)
        .map(|(k, n, mb)| Line::from(vec![Span::styled(format!("  {:<26}", k.label()), if k == Kind::Ours { theme::bold() } else { theme::dim() }), Span::raw(format!("{n:>3}  {mb:>6.0} MB"))]))
        .collect();
    // What the supervisor says the other claude++ windows are doing right now (their tabs, not their processes).
    for p in peers.iter().filter(|p| p.pid != me) {
        let busy = p.tabs.iter().filter(|t| t.busy).count();
        let mb: u64 = p.tabs.iter().map(|t| t.mb).sum();
        head.push(Line::styled(format!("  claude++ {}: {} tab{}, {busy} busy, {mb} MB", p.pid, p.tabs.len(), if p.tabs.len() == 1 { "" } else { "s" }), theme::dim()));
    }
    let cursor = ui.cursor.min(r.rows.len() - 1);
    let body = r
        .rows
        .iter()
        .enumerate()
        .map(|(i, x)| {
            panel::row(i == cursor, vec![
                Span::styled(format!("{:<8}", x.kind.short()), if x.kind == Kind::Ours { theme::ok() } else { theme::dim() }),
                Span::raw(format!("{:>6} {:>6.0} MB  ", x.pid, x.mb)),
                Span::styled(x.cmd.clone(), theme::dim()),
            ])
        })
        .collect();
    let tail = vec![Line::styled(format!("  memory is {}; only \"ours\" is controlled by claude++, the rest is shown", r.source), theme::dim())];
    View::new(format!("Processes · {} MB", r.rows.iter().map(|x| x.mb).sum::<f64>().round()), body, cursor, keys).with_head(head).with_tail(tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(rows: &[(u32, u32, &str)]) -> Vec<Proc> {
        let text: String = rows.iter().map(|(p, pp, c)| format!("{p:>6} {pp:>6} {c}\n")).collect();
        parse_ps(&text)
    }

    #[test]
    fn parses_padded_columns_and_a_command_with_spaces() {
        let p = ps(&[(7, 1, "/Applications/Codex (Aperitif).app/x --type=gpu")]);
        assert_eq!((p[0].pid, p[0].ppid), (7, 1));
        assert_eq!(p[0].cmd, "/Applications/Codex (Aperitif).app/x --type=gpu");
    }

    #[test]
    fn sorts_every_process_by_who_owns_it() {
        let p = ps(&[
            (100, 50, "target/debug/claudeplusplus -c"),
            (101, 100, "claude -p --input-format stream-json"),
            (102, 101, "node /opt/homebrew/bin/codegraph serve --mcp"),
            (200, 1, "claude bg-pty-host --bg-pty-host /tmp/x.sock 200 50 --"),
            (201, 1, "/Users/u/.local/share/claude/versions/2.1.319 --session-id abc"),
            (202, 98101, "claude bg-spare --bg-spare /tmp/x.claim.sock"),
            (203, 1, "/Users/u/.local/bin/claude daemon run --origin transient"),
            (204, 1, "claude -c"),
            (300, 1, "/Applications/ChatGPT.app/x/codex app-server -c mcp_servers.x.enabled=true"),
            (301, 300, "node /opt/homebrew/bin/codegraph serve --mcp"),
            (400, 1, "/x/target/debug/claudeplusplus -c"),
            (401, 400, "claude -p --input-format stream-json"),
            (402, 401, "/Users/u/.caveman/bin/caveman-mcp"),
            (500, 1, "node /opt/homebrew/bin/codegraph serve --mcp"),
            (600, 1, "/usr/sbin/cfprefsd agent"),
            (601, 1, "/Applications/Docker.app/x/docker-agent serve api --mcp-oauth-redirect-url=http://x"),
            (602, 1, "/x/bin/python /Users/u/.local/bin/comfy-mcp"),
            (700, 100, "/x/target/debug/claudeplusplus supervisor"),
            (701, 1, "/x/claudeplusplus supervisor"),
            (702, 700, "node /opt/homebrew/bin/codegraph serve --mcp"),
        ]);
        use Kind::*;
        assert_eq!(
            classify(&p, 100),
            vec![Ours, Ours, Ours, PtyHost, Claude, Spare, Daemon, Claude, Other, Other, Peer, Peer, Peer, Mcp, Other, Other, Mcp, Supervisor, Supervisor, Supervisor]
        );
    }

    #[test]
    fn the_spare_worker_hint_needs_real_memory_and_a_worker_nobody_uses() {
        let row = |kind, mb, cmd: &str| Row { pid: 1, kind, mb, cmd: cmd.into() };
        let r = |rows| Report { rows, source: "footprint" };
        let spare = |mb| row(Kind::Spare, mb, "claude bg-spare --bg-spare /tmp/x");
        assert_eq!(spare_hint(&r(vec![])), None);
        assert_eq!(spare_hint(&r(vec![row(Kind::Daemon, 500.0, "claude daemon run")])), None, "a daemon with no spare worker is not the case");
        assert_eq!(spare_hint(&r(vec![spare(100.0)])), None, "too small to mention");
        let h = spare_hint(&r(vec![row(Kind::Daemon, 80.0, "claude daemon run"), spare(240.0), row(Kind::Spare, 30.0, "claude bg-pty-host --bg-pty-host /tmp/y"), spare(60.0)])).unwrap();
        assert!(h.contains("410 MB") && h.contains("2 idle spare workers") && h.contains("/procs"), "{h}");
    }

    #[test]
    fn a_parent_loop_does_not_hang() {
        let p = ps(&[(10, 11, "claude"), (11, 10, "claude")]);
        assert_eq!(classify(&p, 999), vec![Kind::Claude, Kind::Claude]);
    }
}
