//! Memory: what the processes hold, the budget across tabs, and the link to the supervisor.

use super::*;

impl App {
    /// Tell the supervisor what the tabs are doing and which processes are ours (it dedups, so this is cheap to
    /// call every tick); connect, or reconnect every 10 s, when there is no connection. Never blocks: the
    /// connecting happens on a thread. `CLAUDEPP_SUPERVISOR=off` keeps the window off the shared supervisor.
    pub(crate) fn report_supervisor(&mut self) {
        if cfg!(test) {
            return;
        }
        let off = std::env::var("CLAUDEPP_SUPERVISOR").is_ok_and(|v| v == "off");
        if self.sup.as_ref().is_some_and(|c| !c.alive()) {
            self.sup = None;
            share::set_hub(None);
            self.peers.clear();
        }
        // On its own supervisor, it keeps looking for the shared one (a stopped one is started again with `/supervisor`).
        let own = self.sup.as_ref().is_some_and(|c| c.is_private());
        if (self.sup.is_none() || (own && !off)) && !self.sup_connecting && self.sup_tried.elapsed() > Duration::from_secs(10) {
            (self.sup_connecting, self.sup_tried) = (true, Instant::now());
            let (tx, paths) = (self.tx.clone(), self.sup_paths.clone());
            thread::spawn(move || {
                // The shared supervisor when there is one; else one of this window's own, so it is managed either way.
                // One that does not measure is from before the supervisor took that over from the windows: it would leave
                // this window with no budget and no idle-suspend, so the window uses its own until that one is replaced.
                let shared = if off { None } else { supervisor::Client::connect_or_start(&paths, std::process::id(), tx.clone(), AppEvent::Sup).filter(supervisor::Client::measures) };
                let c = shared.or_else(|| if own { None } else { supervisor::Client::private(std::process::id(), tx.clone(), AppEvent::Sup) });
                let _ = tx.send(AppEvent::Supervisor(c.map(Arc::new)));
            });
        }
        let Some(c) = &self.sup else { return };
        c.window(self.focus, self.last_input.elapsed().as_secs().min(3600) / 30 * 30);
        c.tabs(self.tab_report(), self.sessions.iter().filter_map(|s| s.backend.as_ref().map(|b| b.pid)).collect());
    }

    /// The memory budget (`which` 0) or the idle limit (1): the value this run set (a flag, or `/config` here), else what
    /// `/config` saved, else `default`. The supervisor holds the value that counts; this is what to show for it.
    pub(crate) fn limit(&self, which: usize, key: &str, default: u64) -> u64 {
        let set = if which == 0 { self.limits.0 } else { self.limits.1 };
        set.or_else(|| self.config_path.as_deref().and_then(|p| config::get_str(p, key)).and_then(|v| v.parse().ok())).unwrap_or(default)
    }

    /// Hand the supervisor the budget and the idle limit this run set, if it did (it reads the saved ones itself).
    pub(crate) fn send_limits(&self) {
        let Some(c) = self.sup.as_ref().filter(|c| c.alive() && c.manages()) else { return };
        if let Some(mb) = self.limits.0 {
            c.set("budget_mb", &mb.to_string());
        }
        if let Some(s) = self.limits.1 {
            c.set("idle_s", &s.to_string());
        }
    }

    /// What each tab is, for the supervisor to judge: its process, whether it is busy, how long it has been idle (in 30 s
    /// steps: an idle window must not make a message every second), and whether it may be stopped now (`safe`; `safe_front`
    /// for the tab in front, which goes only when the window is found to be away). It does not say what a tab holds:
    /// the supervisor measures that.
    pub(crate) fn tab_report(&self) -> Vec<serde_json::Value> {
        self.sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let idle = s.last_active.elapsed().as_secs().min(3600) / 30 * 30;
                json!({"id": s.id, "pid": s.backend.as_ref().map(|b| b.pid), "busy": s.busy, "cwd": s.cwd.as_deref().unwrap_or(&self.cwd), "idle_s": idle, "safe": i != self.active && s.can_suspend(), "safe_front": i == self.active && s.can_suspend()})
            })
            .collect()
    }

    /// The supervisor's memory budget covers every claude++ window, so it decides which tab goes; this window only
    /// says no when the tab is not stoppable any more (you switched to it, or it started working).
    pub(crate) fn suspend_for_budget(&mut self, tab: u64, why: &str, front: bool, limit_s: u64) {
        let Some(i) = self.sessions.iter().position(|s| s.id == tab) else { return };
        if i == self.active {
            // The tab in front goes only when the supervisor found this window to be in the background, and nothing here
            // says otherwise: the terminal has focus, or you typed a moment ago.
            // The report is up to ~40 s old (30 s steps plus the heartbeat): the tab must still be quiet, not only have been.
            let quiet = Duration::from_secs(30);
            // A terminal that has focus keeps it unless you have been away five times the idle time (as the supervisor judged).
            let here = self.focus == Some(true) && self.last_input.elapsed() < Duration::from_secs(limit_s * 5);
            if !front || here || self.last_input.elapsed() < quiet || self.sessions[i].last_active.elapsed() < quiet {
                return;
            }
        }
        if self.sessions[i].suspend() {
            let what = if front { "window in the background" } else { "memory budget" };
            self.sessions[i].note(format!("{what} ({why}): claude process stopped to free memory; it resumes on your next message"));
        }
    }

    /// What the supervisor measured for this window and its tabs (it sends them with the peers), and what to say about it.
    pub(crate) fn on_mem(&mut self) {
        let me = std::process::id();
        if let Some(mine) = self.peers.iter().find(|p| p.pid == me) {
            self.ui_mem_kb = mine.ui_mb * 1024;
            for s in &mut self.sessions {
                s.mem_kb = s.backend.as_ref().and_then(|b| mine.tabs.iter().find(|t| t.pid == Some(b.pid))).map_or(0, |t| t.mb * 1024);
            }
        }
        // A very long conversation: say once what it costs and what Claude Code can do about it.
        for s in &mut self.sessions {
            if s.ctx_tokens > LONG_CONTEXT && self.autocompact_k == 0 && !std::mem::replace(&mut s.long_hinted, true) {
                s.note(format!("this conversation is {}k tokens, and every turn re-reads all of it from the prompt cache. `/compact` summarizes it now; `/config` → \"Compact the conversation at\" has Claude Code do that earlier (it is 967k by default).", s.ctx_tokens / 1000));
            }
        }
        // A full-profile tab carries MCP servers and hooks: say once what `lean` would save.
        for s in &mut self.sessions {
            if s.profile == "full" && s.mem_kb > HEAVY_TAB_KB && !std::mem::replace(&mut s.lean_hinted, true) {
                let mb = s.mem_kb / 1024;
                s.note(format!("this tab's claude process uses {mb} MB; `/profile lean` runs a single process of about 220 MB (core tools only, no MCP servers or user hooks). `/new lean` opens one."));
            }
        }
    }
}

/// Say at most once a day, from a thread (`ps` and `footprint` take a moment), if Claude Code's own idle workers hold
/// real memory. The marker file is touched only when something was said.
pub(crate) fn spare_hint_once_a_day(tx: &Sender<AppEvent>) {
    let Some(marker) = cache_path("spare-hint") else { return };
    let said_today = std::fs::metadata(&marker).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|e| e < Duration::from_secs(24 * 3600)));
    if said_today {
        return;
    }
    let tx = tx.clone();
    thread::spawn(move || {
        if let Some(note) = procs::report().and_then(|r| procs::spare_hint(&r)) {
            if let Some(d) = marker.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(&marker, "");
            let _ = tx.send(AppEvent::Hint(note));
        }
    });
}

/// Footprint (KB) per root pid including its child processes. macOS `footprint` reports what the
/// process really holds (RSS overstated it ~2x in our measurements); elsewhere falls back to RSS.
pub(crate) fn tree_mem_kb(roots: &[u32]) -> Vec<(u32, u64)> {
    let mut kids: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    for p in procs::snapshot() {
        kids.entry(p.ppid).or_default().push(p.pid);
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

pub(crate) fn footprint_kb(pids: &[u32]) -> Vec<(u32, u64)> {
    let args: Vec<String> = pids.iter().flat_map(|p| ["-p".to_string(), p.to_string()]).collect();
    match std::process::Command::new("footprint").args(&args).output() {
        Ok(o) => parse_footprint(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => vec![],
    }
}

/// Lines look like `claude [1234]: 64-bit    Footprint: 245 MB (16384 bytes per page)`.
pub(crate) fn parse_footprint(out: &str) -> Vec<(u32, u64)> {
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
pub(crate) fn rss_kb(pids: &[u32]) -> Vec<(u32, u64)> {
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

/// A full-profile tab above this is worth suggesting the lean profile for.
pub(crate) const HEAVY_TAB_KB: u64 = 400 * 1024;

/// Tokens of context after which a conversation is said to be expensive to keep (once per tab).
pub(crate) const LONG_CONTEXT: u64 = 400_000;
