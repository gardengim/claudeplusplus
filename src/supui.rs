//! `/supervisor`: a dashboard for the supervisor. What it is doing (state, uptime, counters), memory against the
//! budget with a short history, every window and its tabs, the shared MCP servers it runs, and its recent log;
//! and the ways to manage it: three settings, Restart / Stop / Start, and a restart for each shared MCP server.
//! It only ever acts on itself and on what it started: the processes of Claude Code are not here.

use crate::panel::{self, View};
use crate::supervisor::Peer;
use crate::theme;
use crate::ui::{tilde, truncate, wrap};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::time::{Duration, Instant, SystemTime};

pub struct Setting {
    /// The name it has in the supervisor's `set` message; saved in `config.json` as `supervisor_<key>`.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub choices: &'static [(&'static str, &'static str)],
}

pub const SETTINGS: [Setting; 3] = [
    Setting {
        key: "grace_s",
        label: "Stays after the last window",
        help: "How long it keeps running once every claude++ window has closed. Shared MCP servers stop with it.",
        choices: &[("20", "20 s"), ("300", "5 min"), ("3600", "1 hour"), ("always", "always")],
    },
    Setting {
        key: "cleanup",
        label: "Clean up after a crash",
        help: "When a window dies without saying goodbye, stop the claude processes it started and left running. Only those, never one claude++ did not start.",
        choices: &[("on", "on"), ("off", "off")],
    },
    Setting {
        key: "budget",
        label: "Memory budget",
        help: "Applies the smallest budget any window set (/config): over it, the longest-idle tab's claude is stopped. Paused leaves every tab running.",
        choices: &[("on", "enforced"), ("paused", "paused")],
    },
];


/// How this window stands with the supervisor.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Link {
    /// Connected, and it answers management messages.
    Live,
    /// Connected, an older build: it can only be told to leave by signal.
    Old,
    /// Not connected. `stopped`: on purpose, so nothing starts it. `env`: `CLAUDEPP_SUPERVISOR=off`.
    Down { stopped: bool, env: bool },
}

/// What the cursor can be on: the selectable lines of the dashboard, top to bottom.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Row {
    /// A shared MCP server, by its place in the status.
    Mcp(usize),
    Setting(usize),
    /// The row of buttons.
    Actions,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Button {
    Restart,
    Stop,
    Start,
}

impl Button {
    fn label(self) -> &'static str {
        match self {
            Button::Restart => "Restart",
            Button::Stop => "Stop",
            Button::Start => "Start",
        }
    }
    fn help(self) -> &'static str {
        match self {
            Button::Restart => "Windows reconnect within seconds and a new supervisor starts from the current claude++ binary.",
            Button::Stop => "It leaves and stays stopped until you Start it; shared MCP servers end and run in each window instead.",
            Button::Start => "Starts it now; it also starts by itself when a window needs it.",
        }
    }
}

pub enum Act {
    None,
    Close,
    Change(usize, isize),
    McpRestart(usize),
    Restart,
    Stop,
    Start,
}

pub struct Ui {
    pub sel: usize,
    /// The button under the cursor when the buttons are the row.
    pub btn: usize,
    pub status: Option<Value>,
    pub asked: Instant,
    /// Total memory in all windows, one sample per answer: the sparkline.
    pub history: Vec<u64>,
}

impl Default for Ui {
    fn default() -> Ui {
        Ui { sel: 0, btn: 0, status: None, asked: Instant::now(), history: vec![] }
    }
}

const HISTORY: usize = 120;

pub fn buttons(link: Link) -> Vec<Button> {
    match link {
        Link::Live | Link::Old => vec![Button::Restart, Button::Stop],
        Link::Down { env: true, .. } => vec![],
        Link::Down { .. } => vec![Button::Start],
    }
}

/// The rows the link allows, in order.
pub fn rows(link: Link, status: Option<&Value>) -> Vec<Row> {
    let settings = (0..SETTINGS.len()).map(Row::Setting);
    match link {
        Link::Live => (0..status.map_or(0, |s| s["mcp"].as_array().map_or(0, Vec::len))).map(Row::Mcp).chain(settings).chain([Row::Actions]).collect(),
        Link::Old => vec![Row::Actions],
        Link::Down { env: true, .. } => settings.collect(),
        Link::Down { .. } => settings.chain([Row::Actions]).collect(),
    }
}

impl Ui {
    /// A new answer from the supervisor.
    pub fn answered(&mut self, v: Value) {
        if let Some(mb) = v["total_mb"].as_u64() {
            self.history.push(mb);
            let extra = self.history.len().saturating_sub(HISTORY);
            self.history.drain(..extra);
        }
        self.status = Some(v);
    }

    pub fn key(&mut self, k: KeyEvent, link: Link, rows: &[Row]) -> Act {
        if k.code == KeyCode::Esc || panel::is_ctrl_c(&k) {
            return Act::Close;
        }
        if rows.is_empty() {
            return Act::None;
        }
        self.sel = self.sel.min(rows.len() - 1);
        let row = rows[self.sel];
        let btns = buttons(link);
        self.btn = self.btn.min(btns.len().saturating_sub(1));
        match (k.code, row) {
            (KeyCode::Up, _) => self.sel = panel::cycle(self.sel, rows.len(), true),
            (KeyCode::Down, _) => self.sel = panel::cycle(self.sel, rows.len(), false),
            (KeyCode::Left | KeyCode::Right, Row::Setting(i)) => return Act::Change(i, if k.code == KeyCode::Left { -1 } else { 1 }),
            (KeyCode::Left | KeyCode::Right, Row::Actions) if !btns.is_empty() => self.btn = panel::cycle(self.btn, btns.len(), k.code == KeyCode::Left),
            (KeyCode::Enter, Row::Setting(i)) => return Act::Change(i, 1),
            (KeyCode::Enter, Row::Mcp(i)) => return Act::McpRestart(i),
            (KeyCode::Enter, Row::Actions) => {
                return match btns.get(self.btn) {
                    Some(Button::Restart) => Act::Restart,
                    Some(Button::Stop) => Act::Stop,
                    Some(Button::Start) => Act::Start,
                    None => Act::None,
                };
            }
            _ => {}
        }
        Act::None
    }
}

/// `2h 10m`, `5m`, `40s`.
fn span(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        86400.. => format!("{}d {}h", s / 86400, s % 86400 / 3600),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

fn plural(n: u64, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// `1,204`.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `▁▃▅▇` for the last `width` values, scaled to `cap` or their own peak if higher.
fn spark(vals: &[u64], cap: u64, width: usize) -> String {
    const STEPS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let from = vals.len().saturating_sub(width);
    let top = vals.iter().copied().max().unwrap_or(0).max(cap).max(1);
    vals[from..].iter().map(|&v| STEPS[((v * 7 + top / 2) / top).min(7) as usize]).collect()
}

/// ` Title  extra ────────`: a rule across the dashboard that names what is under it.
fn section(title: &str, extra: &str, width: usize) -> Line<'static> {
    let used = title.chars().count() + extra.chars().count() + 4;
    Line::from(vec![
        Span::styled(format!(" {title} "), theme::accent().add_modifier(ratatui::style::Modifier::BOLD)),
        Span::styled(extra.to_string(), theme::dim()),
        Span::styled(format!(" {}", "─".repeat(width.saturating_sub(used))), theme::dim()),
    ])
}

/// A label and its value, for the figures in a line of tiles.
fn tile(label: &str, value: String) -> [Span<'static>; 3] {
    [Span::styled(format!("{label} "), theme::dim()), Span::styled(value, theme::bold()), Span::styled("   ", theme::dim())]
}

/// The colour of a gauge as it fills.
fn level(frac: f64) -> Style {
    if frac >= 0.9 {
        theme::danger()
    } else if frac >= 0.7 {
        theme::attention()
    } else {
        theme::ok()
    }
}

fn gauge(frac: f64, width: usize, hue: Style) -> Vec<Span<'static>> {
    let (on, off) = crate::motion::look().bar(frac, width);
    vec![Span::styled(on, hue), Span::styled(off, theme::dim())]
}

struct TabRow {
    cwd: String,
    busy: bool,
    running: bool,
    mb: u64,
    idle_s: Option<u64>,
}

struct Win {
    pid: u32,
    tabs: Vec<TabRow>,
    /// Processes it asked the supervisor to watch; `None` when only the peers' list is known.
    owned: Option<u64>,
    mb: u64,
    budget_mb: u64,
}

/// The windows, from the supervisor's own answer or else from the list every window gets of its peers.
fn wins(status: Option<&Value>, peers: &[Peer]) -> Vec<Win> {
    if let Some(list) = status.and_then(|s| s["windows"].as_array()) {
        return list
            .iter()
            .map(|w| Win {
                pid: w["pid"].as_u64().unwrap_or(0) as u32,
                tabs: w["tabs"].as_array().into_iter().flatten().map(|t| TabRow { cwd: t["cwd"].as_str().unwrap_or("").into(), busy: t["busy"].as_bool().unwrap_or(false), running: t["pid"].is_u64(), mb: t["mb"].as_u64().unwrap_or(0), idle_s: t["idle_s"].as_u64() }).collect(),
                owned: w["owned"].as_u64(),
                mb: w["mb"].as_u64().unwrap_or(0),
                budget_mb: w["budget_mb"].as_u64().unwrap_or(0),
            })
            .collect();
    }
    peers
        .iter()
        .map(|p| {
            let tabs: Vec<TabRow> = p.tabs.iter().map(|t| TabRow { cwd: t.cwd.clone(), busy: t.busy, running: t.pid.is_some(), mb: t.mb, idle_s: None }).collect();
            Win { pid: p.pid, mb: tabs.iter().map(|t| t.mb).sum(), tabs, owned: None, budget_mb: 0 }
        })
        .collect()
}

/// The first lines: whether it runs, and anything that needs doing about it.
fn hero(link: Link, status: Option<&Value>, pid: Option<u32>, exe_mtime: u64) -> Vec<Line<'static>> {
    let bold = |s: Style| s.add_modifier(ratatui::style::Modifier::BOLD);
    match (link, status) {
        (Link::Live, Some(s)) => {
            let stale = s["exe_mtime"].as_u64().is_some_and(|m| m != exe_mtime && exe_mtime != 0);
            let mut out = vec![Line::from(vec![
                Span::styled(" ● RUNNING", bold(theme::ok())),
                Span::styled(format!("   pid {} · up {} · protocol {}", s["pid"], span(s["up_s"].as_u64().unwrap_or(0)), s["v"]), theme::dim()),
            ])];
            if stale {
                out.push(Line::from(Span::styled("   ⚠ started from an older claude++ binary: Restart picks up this one", theme::attention())));
            }
            out
        }
        (Link::Live, None) => vec![Line::from(vec![Span::styled(" ● RUNNING", bold(theme::ok())), Span::styled("   asking it…", theme::dim())])],
        (Link::Old, _) => vec![
            Line::from(vec![Span::styled(" ● RUNNING", bold(theme::attention())), Span::styled(pid.map_or(String::new(), |p| format!("   pid {p}")), theme::dim())]),
            Line::from(Span::styled("   an older build: it cannot be inspected or configured; Restart replaces it with this one", theme::attention())),
        ],
        (Link::Down { env: true, .. }, _) => vec![Line::from(vec![Span::styled(" ○ OFF", bold(theme::dim())), Span::styled("   CLAUDEPP_SUPERVISOR=off: this window has a supervisor of its own and shares nothing with other windows", theme::dim())])],
        (Link::Down { stopped: true, .. }, _) => vec![
            Line::from(Span::styled(" ○ STOPPED", bold(theme::dim()))),
            Line::from(Span::styled("   nothing starts it until you Start it; each window manages its own memory and MCP servers meanwhile", theme::dim())),
        ],
        (Link::Down { .. }, _) => vec![
            Line::from(Span::styled(" ○ NOT RUNNING", bold(theme::dim()))),
            Line::from(Span::styled("   it starts by itself in a few seconds, or Start it now", theme::dim())),
        ],
    }
}

/// `value` is what a setting is now; `me` is this window's pid; `exe_mtime` this binary's.
#[allow(clippy::too_many_arguments)]
pub fn view(ui: &Ui, link: Link, pid: Option<u32>, peers: &[Peer], me: u32, exe_mtime: u64, value: impl Fn(usize) -> String, width: usize) -> View {
    let w = width.saturating_sub(4).max(40);
    let status = ui.status.as_ref().filter(|_| link == Link::Live);
    let rows = rows(link, status);
    let sel = ui.sel.min(rows.len().saturating_sub(1));
    let mut lines: Vec<Line<'static>> = hero(link, status, pid, exe_mtime);
    let mut focus = 0;
    let mut help = String::new();
    let wl = wins(status, peers);
    let live = !matches!(link, Link::Down { .. });

    // Tiles: the figures at a glance.
    if live {
        let tabs: u64 = wl.iter().map(|x| x.tabs.len() as u64).sum();
        let busy: u64 = wl.iter().flat_map(|x| &x.tabs).filter(|t| t.busy).count() as u64;
        let running: u64 = wl.iter().flat_map(|x| &x.tabs).filter(|t| t.running).count() as u64;
        let mcp_n = status.and_then(|s| s["mcp"].as_array()).map_or(0, Vec::len) as u64;
        let mut spans = vec![Span::raw(" ")];
        spans.extend(tile("windows", wl.len().to_string()));
        spans.extend(tile("tabs", format!("{tabs} ({busy} working, {running} with a claude process)")));
        if status.is_some() {
            spans.extend(tile("shared MCP servers", mcp_n.to_string()));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(spans));
        if let Some(c) = status.map(|s| &s["counts"]) {
            let n = |k: &str| c[k].as_u64().unwrap_or(0);
            let mut spans = vec![Span::styled(" since it started  ", theme::dim())];
            spans.extend(tile("tabs stopped by the budget", n("suspended").to_string()));
            spans.extend(tile("processes cleaned up", n("reaped").to_string()));
            spans.extend(tile("MCP messages", thousands(n("mcp_msgs"))));
            spans.extend(tile("windows served", n("windows_ever").to_string()));
            lines.push(Line::from(spans));
        }
    }

    // Memory: the gauge against the budget, and how it has moved.
    if let Some(s) = status {
        let (total, budget) = (s["total_mb"].as_u64().unwrap_or(0), s["budget_mb"].as_u64().unwrap_or(0));
        let enforced = s["settings"]["budget"].as_bool().unwrap_or(true);
        lines.push(Line::raw(""));
        lines.push(section("Memory", &format!("{total} MB in all windows"), w));
        let mut spans = vec![Span::raw("  ")];
        if budget > 0 {
            let frac = total as f64 / budget as f64;
            spans.extend(gauge(frac, 28, level(frac)));
            spans.push(Span::styled(format!("  {total} of {budget} MB · {:.0}%  ", frac * 100.0), level(frac)));
            spans.push(Span::styled(if enforced { "budget enforced" } else { "budget paused" }, if enforced { theme::dim() } else { theme::attention() }));
        } else {
            spans.push(Span::styled("no budget set: a window asks for one with /config → Memory budget; the smallest asked wins", theme::dim()));
        }
        lines.push(Line::from(spans));
        if ui.history.len() >= 2 {
            let peak = ui.history.iter().copied().max().unwrap_or(0);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(spark(&ui.history, budget, 60), theme::accent()),
                Span::styled(format!("  last {} · peak {peak} MB", span(ui.history.len() as u64)), theme::dim()),
            ]));
        }
    }

    // Windows, each with its tabs.
    if live {
        lines.push(Line::raw(""));
        lines.push(section("Windows", &wl.len().to_string(), w));
        let total: u64 = wl.iter().map(|x| x.mb).sum::<u64>().max(1);
        for x in &wl {
            let you = if x.pid == me { "  (this one)" } else { "" };
            let mut spans = vec![Span::styled(format!("  pid {}{you}", x.pid), theme::bold()), Span::raw("   ")];
            spans.extend(gauge(x.mb as f64 / total as f64, 10, theme::accent()));
            spans.push(Span::styled(format!("  {} MB", x.mb), theme::bold()));
            let mut facts = vec![plural(x.tabs.len() as u64, "tab")];
            facts.extend(x.owned.map(|o| format!("{o} watched")));
            facts.push(if x.budget_mb > 0 { format!("budget {} MB", x.budget_mb) } else { "no budget".into() });
            spans.push(Span::styled(format!("   {}", facts.join(" · ")), theme::dim()));
            lines.push(Line::from(spans));
            let name_w = w.saturating_sub(34).clamp(12, 40);
            for t in &x.tabs {
                let (mark, hue, state) = match (t.busy, t.running) {
                    (true, _) => ("●", theme::ok(), "working".to_string()),
                    (false, true) => ("○", theme::dim(), t.idle_s.map_or("idle".into(), |s| format!("idle {}", span(s)))),
                    (false, false) => ("◌", theme::dim(), "no process".into()),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("     {mark} "), hue),
                    Span::raw(format!("{:<name_w$}", truncate(&tilde(&t.cwd), name_w))),
                    Span::styled(format!(" {:>6} MB  {state}", t.mb), theme::dim()),
                ]));
            }
        }
    }

    // Shared MCP servers: each one can be restarted.
    if let Some(s) = status {
        let mcp = s["mcp"].as_array().cloned().unwrap_or_default();
        lines.push(Line::raw(""));
        lines.push(section("Shared MCP servers", &mcp.len().to_string(), w));
        if mcp.is_empty() {
            lines.push(Line::from(Span::styled("   none running · /config → Share MCP servers makes windows share one copy of each", theme::dim())));
        }
        for (i, m) in mcp.iter().enumerate() {
            let on = rows.get(sel) == Some(&Row::Mcp(i));
            if on {
                focus = lines.len();
                help = "Enter stops this process; the next message from any window starts it again.".into();
            }
            let name = format!("{} ({})", m["command"].as_str().unwrap_or("?"), tilde(m["cwd"].as_str().unwrap_or("")));
            let facts = format!("  pid {} · {} MB · {} · started {}×", m["pid"].as_u64().map_or("-".into(), |p| p.to_string()), m["mb"].as_u64().unwrap_or(0), plural(m["windows"].as_u64().unwrap_or(0), "window"), m["starts"].as_u64().unwrap_or(0));
            lines.push(panel::row(on, vec![Span::styled(truncate(&name, w.saturating_sub(facts.chars().count() + 4)), if on { theme::bold() } else { Default::default() }), Span::styled(facts, theme::dim())]));
        }
    }

    // Settings and buttons: the controls.
    if rows.iter().any(|r| matches!(r, Row::Setting(_) | Row::Actions)) {
        lines.push(Line::raw(""));
        lines.push(section("Controls", "", w));
    }
    let label_w = SETTINGS.iter().map(|s| s.label.len()).max().unwrap_or(0) + 2;
    for (n, row) in rows.iter().enumerate() {
        let on = n == sel;
        let name = |t: String| Span::styled(t, if on { theme::bold() } else { Default::default() });
        match *row {
            Row::Mcp(_) => continue,
            Row::Setting(i) => {
                let s = &SETTINGS[i];
                let now = value(i);
                let mut spans = vec![name(format!("{:<label_w$}", s.label))];
                spans.extend(s.choices.iter().map(|(v, label)| theme::chip_in(label, *v == now, *v == now && on, true, theme::bold())));
                if on {
                    focus = lines.len();
                    help = s.help.into();
                }
                lines.push(panel::row_with(on, false, spans));
            }
            Row::Actions => {
                let btns = buttons(link);
                let at = ui.btn.min(btns.len().saturating_sub(1));
                let mut spans = vec![name(format!("{:<label_w$}", "Supervisor"))];
                spans.extend(btns.iter().enumerate().map(|(b, x)| theme::chip_in(x.label(), false, on && b == at, true, theme::bold())));
                if on {
                    focus = lines.len();
                    help = btns.get(at).map_or("", |b| b.help()).into();
                }
                lines.push(panel::row_with(on, false, spans));
            }
        }
    }

    // Activity: what it logged lately.
    if let Some(log) = status.and_then(|s| s["log"].as_array()).filter(|l| !l.is_empty()) {
        lines.push(Line::raw(""));
        lines.push(section("Activity", "", w));
        for l in log.iter().filter_map(|l| l.as_str()) {
            let (secs, msg) = l.split_once(' ').unwrap_or(("", l));
            let when = secs.parse::<u64>().map_or(String::new(), |s| {
                let (h, m, sec) = crate::queue::local_hms(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
                format!("{h:02}:{m:02}:{sec:02}")
            });
            let hue = if ["stopping", "killing", "without saying goodbye", "asking instance", "asked to shut down"].iter().any(|k| msg.contains(k)) { theme::attention() } else { theme::dim() };
            lines.push(Line::from(vec![Span::styled(format!("   {when}  "), theme::dim()), Span::styled(truncate(msg, w.saturating_sub(14)), hue)]));
        }
    }

    // The help for the row under the cursor stays pinned: the dashboard scrolls, this does not move.
    let mut tail: Vec<Line<'static>> = wrap(&help, w).into_iter().take(2).map(|l| Line::from(Span::styled(format!(" {l}"), theme::dim()))).collect();
    tail.resize(2, Line::raw(""));
    let footer = theme::hints(&[("↑↓←→", "move"), ("⏎", "change or run"), ("esc", "close")]);
    View::new("Supervisor", lines, focus, footer).with_tail(tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_rows_follow_what_can_be_done() {
        let st = json!({"mcp": [{}, {}]});
        assert_eq!(rows(Link::Live, Some(&st)), [Row::Mcp(0), Row::Mcp(1), Row::Setting(0), Row::Setting(1), Row::Setting(2), Row::Actions]);
        assert_eq!(rows(Link::Old, None), [Row::Actions], "an older build can only be restarted or stopped");
        assert_eq!(rows(Link::Down { stopped: true, env: false }, None), [Row::Setting(0), Row::Setting(1), Row::Setting(2), Row::Actions]);
        assert_eq!(rows(Link::Down { stopped: false, env: true }, None).len(), 3, "off by the environment: nothing to start");
        assert_eq!((buttons(Link::Live).len(), buttons(Link::Down { stopped: false, env: false })), (2, vec![Button::Start]));
    }

    #[test]
    fn figures_read_the_way_a_dashboard_shows_them() {
        assert_eq!((span(40), span(300), span(7800), span(200_000)), ("40s".into(), "5m".into(), "2h 10m".into(), "2d 7h".into()));
        assert_eq!((thousands(7), thousands(1204), thousands(1_234_567)), ("7".into(), "1,204".into(), "1,234,567".into()));
        assert_eq!(spark(&[0, 50, 100], 100, 10), "▁▅█");
        assert_eq!(spark(&[10, 20, 30, 40], 0, 2), "▆█", "only the last values that fit, scaled to the peak");
    }

    #[test]
    fn the_history_keeps_the_latest_samples() {
        let mut ui = Ui::default();
        for i in 0..(HISTORY as u64 + 5) {
            ui.answered(json!({"total_mb": i}));
        }
        assert_eq!((ui.history.len(), ui.history[0]), (HISTORY, 5));
    }
}
