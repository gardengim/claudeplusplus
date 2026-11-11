//! Full-screen session list: this directory's earlier conversations (Claude Code's transcripts, see
//! history.rs) plus the supervisor's background sessions from every directory (see jobs.rs). Rows that
//! want you come first. `/` starts a filter (`s:work` filters by state), see the end of the highlighted
//! conversation, Enter to open it. Built when opened and dropped when closed.

use crate::history::{self, Summary};
use crate::jobs::{self, Job, State};
use crate::panel::{self, View};
use crate::{theme, ui};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::SystemTime;

const MAX_SESSIONS: usize = 200;
const PREVIEW_TURNS: usize = 8;
const PREVIEW_LINES_PER_TURN: usize = 6;

/// How the list is ordered, cycled with Tab and remembered while claude++ runs.
static GROUP: AtomicU8 = AtomicU8::new(0);
const GROUPS: [&str; 3] = ["by state", "recent", "by folder"];

/// A line of the list: a group heading (its name, text and colour), or a session (index into `rows`).
/// Both can hold the cursor; Enter on a heading folds or unfolds its group.
enum Entry {
    Head(String, String, Style),
    Row(usize),
}

/// The grouping (0 by state, 1 recent, 2 by folder) is shared by every list; the app saves it.
pub fn group() -> u8 {
    GROUP.load(Ordering::Relaxed)
}

pub fn set_group(n: u8) {
    GROUP.store(n % 3, Ordering::Relaxed);
}

pub enum Outcome {
    Stay,
    Close,
    /// Session id, its title and the directory it lives in (`None` = ours).
    Open(String, String, Option<String>),
    /// Open in the tab in front, replacing what it holds (→), instead of in a new tab.
    OpenHere(String, String, Option<String>),
    /// Open a copy (`--fork-session`) so the original is left alone.
    Fork(String, String, Option<String>),
    /// `claude stop <short>`; then open the session here if given.
    Stop(String, Option<(String, String, Option<String>)>),
    /// Interrupt the turn this open tab is working on (session id); the list stays.
    Interrupt(String),
    /// Say something and keep the list.
    Refused(String),
}

/// What one row shows, and nothing else: summaries are clipped and flattened when read.
struct Row {
    id: String,
    /// Supervisor id (`claude attach|stop <short>`) for background sessions.
    short: Option<String>,
    title: Box<str>,
    detail: Box<str>,
    /// Empty when it is the directory the list was opened from.
    cwd: Box<str>,
    state: Option<State>,
    /// The state the supervisor reports, before an open tab overrides it.
    base: Option<State>,
    /// Open in one of our tabs (its state is then the tab's own: needs you, working, or done).
    tab: bool,
    modified: SystemTime,
    size: u64,
}

pub struct Explorer {
    rows: Vec<Row>,
    /// Sessions already open in a tab, with the tab's state.
    open: Vec<(String, State)>,
    pub query: String,
    sel: usize,
    preview: Option<(String, Vec<(bool, String)>)>,
    /// A longer look at the highlighted conversation, for the main area while the side panel follows you.
    peek: Option<(String, Vec<(bool, String)>)>,
    /// Headings folded away (names), while this list is open.
    folded: Vec<String>,
    /// A destructive chord pressed once: (session id, which); the same chord again confirms.
    armed: Option<(String, char)>,
    /// The `/color` of the highlighted conversation, read from its transcript once per row: (id, colour).
    tint: Option<(String, Option<String>)>,
    /// `/` was pressed: typing now edits the filter. Without it, letters do nothing.
    filtering: bool,
    /// Opened before the list was read: the rows come in a moment.
    loading: bool,
}

/// What the lists show, read from disk: slow enough (hundreds of files) that the app does it on a thread.
pub fn load() -> (Vec<Summary>, Vec<Job>) {
    (history::recent_all(MAX_SESSIONS), jobs::load())
}

/// `s` cut to `w` columns with an ellipsis, or padded with spaces up to `w`.
fn fit(s: &str, w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w.saturating_sub(1) && s.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() > w {
            out.push('…');
            used += 1;
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push_str(&" ".repeat(w.saturating_sub(used)));
    out
}

/// A group heading as spans: fold marker and count quiet, the state glyph in its colour, the name plain.
fn head_spans(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut parts = text.splitn(2, ' ');
    let (marker, rest) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (name, count) = rest.rsplit_once(' ').unwrap_or((rest, ""));
    let mut out = vec![Span::styled(format!("{marker} "), theme::dim())];
    match name.split_once(' ') {
        Some((glyph, label)) if glyph.chars().count() == 1 => out.extend([Span::styled(format!("{glyph} "), style), Span::styled(label.to_string(), theme::bold())]),
        _ => out.push(Span::styled(name.to_string(), theme::bold())),
    }
    out.push(Span::styled(format!("  {count}"), theme::dim()));
    out
}

fn merge(here: &str, items: Vec<Summary>, jobs: Vec<Job>) -> Vec<Row> {
    let mut rows: Vec<Row> = items
        .into_iter()
        .map(|s| Row { id: s.id, short: None, title: s.title.into(), detail: s.prompt.into(), cwd: if s.cwd == here { "".into() } else { s.cwd.into() }, state: None, base: None, tab: false, modified: s.modified, size: s.size })
        .collect();
    for j in jobs {
        let cwd: Box<str> = if &*j.cwd == here { "".into() } else { j.cwd };
        match rows.iter_mut().find(|r| r.id == j.session) {
            Some(r) => {
                if !j.name.is_empty() {
                    r.title = j.name;
                }
                (r.short, r.detail, r.state, r.base, r.cwd) = (Some(j.short), j.detail, Some(j.state), Some(j.state), cwd);
                r.modified = r.modified.max(j.modified);
            }
            None => rows.push(Row { id: j.session, short: Some(j.short), title: j.name, detail: j.detail, cwd, state: Some(j.state), base: Some(j.state), tab: false, modified: j.modified, size: 0 }),
        }
    }
    // Sessions that want you, then the rest by recency.
    rows.sort_by_key(|r| (r.state.filter(|s| s.live()).map_or(2, |s| s as u8), std::cmp::Reverse(r.modified)));
    rows.truncate(MAX_SESSIONS);
    rows
}

impl Explorer {
    pub fn new(cwd: &str, open: Vec<(String, State)>) -> Self {
        let (items, jobs) = load();
        Self::from(cwd, items, jobs, open)
    }

    /// Opens at once on what was read last (or empty, and "loading…", when nothing was yet): `reload` fills it in.
    pub fn from_cache(cwd: &str, open: Vec<(String, State)>, last: Option<&(Vec<Summary>, Vec<Job>)>) -> Self {
        match last {
            Some((items, jobs)) => Self::from(cwd, items.clone(), jobs.clone(), open),
            None => {
                let mut e = Self::from(cwd, vec![], vec![], open);
                e.loading = true;
                e
            }
        }
    }

    /// New rows from a read done elsewhere. The cursor stays on its conversation, else goes to `prefer`.
    pub fn reload(&mut self, here: &str, data: &(Vec<Summary>, Vec<Job>), open: Vec<(String, State)>, prefer: Option<String>) {
        let keep = self.cur().map(|i| self.rows[i].id.clone()).or(prefer);
        self.rows = merge(here, data.0.clone(), data.1.clone());
        self.loading = false;
        self.set_open(open);
        self.seek(keep);
    }

    #[cfg(test)]
    pub fn for_bench(items: Vec<Summary>) -> Self {
        Self::from("", items, vec![], vec![])
    }

    fn from(here: &str, items: Vec<Summary>, jobs: Vec<Job>, open: Vec<(String, State)>) -> Self {
        let mut e = Explorer { rows: merge(here, items, jobs), open: vec![], query: String::new(), sel: 0, preview: None, peek: None, folded: vec![], armed: None, tint: None, filtering: false, loading: false };
        e.set_open(open);
        e.sel = e.first_row();
        e
    }

    /// Indices of rows matching every space-separated term of the filter; `s:<state>` terms match the
    /// state label, the rest title, summary, directory and id.
    fn matches(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        let terms: Vec<&str> = q.split_whitespace().collect();
        let mut v: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                let hay = format!("{} {} {} {}", r.title, r.detail, r.cwd, r.id).to_lowercase();
                terms.iter().all(|t| match t.strip_prefix("s:") {
                    Some(st) => r.state.is_some_and(|s| s.label().starts_with(st) || (st == "needs" && s == State::Blocked)),
                    None => hay.contains(t),
                })
            })
            .collect();
        v.sort_by(|&a, &b| {
            let (ka, kb) = (self.heading(&self.rows[a]), self.heading(&self.rows[b]));
            (ka.0, ka.1).cmp(&(kb.0, kb.1)).then(self.rows[b].modified.cmp(&self.rows[a].modified))
        });
        v
    }

    /// Which group a row belongs to: its rank (for ordering), heading text and colour. Empty heading = none.
    fn heading(&self, r: &Row) -> (u8, String, Style) {
        match GROUP.load(Ordering::Relaxed) {
            0 => {
                let (rank, name, style) = match r.state {
                    Some(State::Blocked) => (0, "needs you", theme::attention()),
                    Some(State::Working) => (1, "working", theme::accent()),
                    Some(State::Failed) => (2, "failed", theme::danger()),
                    Some(State::Done) => (3, "done", theme::ok()),
                    Some(State::Stopped) => (4, "stopped", theme::dim()),
                    None => (5, "earlier", theme::dim()),
                };
                let glyph = r.state.map_or("○", State::glyph);
                (rank, format!("{glyph} {name}"), style)
            }
            1 => (0, String::new(), theme::dim()),
            _ => {
                let dir = std::path::Path::new(&*r.cwd).file_name().map(|d| d.to_string_lossy().into_owned());
                (u8::from(dir.is_some()), format!("▤ {}", dir.unwrap_or_else(|| "this folder".into())), theme::bold())
            }
        }
    }

    /// The list as display lines: a heading before each group, and no rows under a folded one.
    fn entries(&self) -> Vec<Entry> {
        let m = self.matches();
        let folded = &self.folded;
        let mut out = Vec::new();
        let mut last = String::new();
        let mut hide = false;
        for (k, &i) in m.iter().enumerate() {
            let (_, name, style) = self.heading(&self.rows[i]);
            if !name.is_empty() && name != last {
                let n = m[k..].iter().take_while(|&&j| self.heading(&self.rows[j]).1 == name).count();
                hide = folded.contains(&name);
                out.push(Entry::Head(name.clone(), format!("{} {name} {n}", if hide { theme::CLOSED } else { theme::OPEN }), style));
                last = name;
            }
            if !hide {
                out.push(Entry::Row(i));
            }
        }
        out
    }

    /// The session under the cursor (`None` on a heading or an empty list).
    fn cur(&self) -> Option<usize> {
        match self.entries().get(self.sel) {
            Some(Entry::Row(i)) => Some(*i),
            _ => None,
        }
    }

    /// Put the cursor back on session `id` after the list was reordered or reread.
    fn seek(&mut self, id: Option<String>) {
        let e = self.entries();
        self.sel = id.and_then(|id| e.iter().position(|x| matches!(x, Entry::Row(i) if self.rows[*i].id == id))).unwrap_or(self.sel.min(e.len().saturating_sub(1)));
    }

    pub fn filtering(&self) -> bool {
        self.filtering
    }

    /// Leave the filter, keeping the cursor on the conversation it was on (the top one if nothing matched).
    pub fn clear_filter(&mut self) {
        if !self.filtering && self.query.is_empty() {
            return;
        }
        let keep = self.cur().map(|i| self.rows[i].id.clone());
        (self.filtering, self.query) = (false, String::new());
        match keep {
            Some(id) => self.select(&id),
            None => self.sel = self.first_row(),
        }
    }

    /// Put the cursor on this conversation, if the list has it.
    pub fn select(&mut self, id: &str) {
        self.seek(Some(id.to_string()));
    }

    /// Folded headings are shared between the side panel and the full list, so the app hands them in.
    pub fn set_folds(&mut self, folds: Vec<String>) {
        self.folded = folds;
    }

    pub fn folds(&self) -> &[String] {
        &self.folded
    }

    /// The first session, skipping headings (after typing in the filter).
    fn first_row(&self) -> usize {
        self.entries().iter().position(|x| matches!(x, Entry::Row(_))).unwrap_or(0)
    }

    /// Enter on a heading: fold or unfold the group.
    fn toggle_fold(&mut self) {
        let e = self.entries();
        if let Some(Entry::Head(name, ..)) = e.get(self.sel) {
            match self.folded.iter().position(|x| x == name) {
                Some(p) => drop(self.folded.remove(p)),
                None => self.folded.push(name.clone()),
            }
        }
    }

    /// Tab: by state, recent, by folder. The cursor stays on the same session.
    fn cycle_group(&mut self) {
        let keep = self.cur().map(|i| self.rows[i].id.clone());
        GROUP.store((GROUP.load(Ordering::Relaxed) + 1) % 3, Ordering::Relaxed);
        self.seek(keep);
    }

    /// Update which sessions are open in tabs, and what state each tab is in.
    pub fn set_open(&mut self, open: Vec<(String, State)>) {
        for r in &mut self.rows {
            (r.tab, r.state) = (false, r.base);
        }
        for (id, st) in &open {
            if let Some(r) = self.rows.iter_mut().find(|r| r.id == *id) {
                (r.tab, r.state) = (true, Some(*st));
            }
        }
        self.open = open;
    }

    /// The highlighted session and whether it is open in a tab.
    pub fn selected(&self) -> Option<(String, bool)> {
        self.cur().map(|i| (self.rows[i].id.clone(), self.rows[i].tab))
    }

    /// The highlighted conversation's `/color`, for the UI to wear while it is browsed.
    pub fn color(&mut self) -> Option<String> {
        let (id, _) = self.selected()?;
        if self.tint.as_ref().is_none_or(|t| t.0 != id) {
            self.tint = Some((id.clone(), history::find(&id).and_then(|p| history::session_color(&p))));
        }
        self.tint.as_ref().and_then(|t| t.1.clone())
    }

    /// The end of the highlighted (closed) conversation, to show in the main area.
    pub fn peek_lines(&mut self, width: usize) -> Vec<Line<'static>> {
        let Some((id, _)) = self.selected() else { return vec![] };
        if self.peek.as_ref().map(|p| p.0.as_str()) != Some(id.as_str()) {
            let turns = history::find(&id).map(|p| history::preview(&p, 40)).unwrap_or_default();
            self.peek = Some((id.clone(), turns));
        }
        let mut out = vec![Line::styled(format!("preview · {} · {} opens it in a new tab", &id[..id.len().min(8)], theme::key_glyph("enter")), theme::dim())];
        for (user, text) in &self.peek.as_ref().unwrap().1 {
            out.push(Line::raw(""));
            let (lead, style) = if *user { ("❯ ", theme::accent()) } else { ("● ", Style::new()) };
            for (i, l) in ui::wrap(text, width.saturating_sub(2)).into_iter().take(12).enumerate() {
                out.push(Line::styled(format!("{}{l}", if i == 0 { lead } else { "  " }), style));
            }
        }
        out
    }

    /// The docked column: one compact line per session under group headings, only what fits around the cursor.
    pub fn dock_lines(&mut self, width: usize, height: usize, focused: bool) -> Vec<Line<'static>> {
        let entries = self.entries();
        self.sel = self.sel.min(entries.len().saturating_sub(1));
        let head = if self.filtering {
            Line::from(vec![Span::styled(" / ", theme::accent()), Span::raw(self.query.clone()), Span::styled(" ", theme::selected())])
        } else {
            Line::from(vec![Span::styled(" Sessions", theme::dim()), Span::styled(format!(" · {}", GROUPS[GROUP.load(Ordering::Relaxed) as usize]), theme::dim())])
        };
        let mut out = vec![head];
        let keys = if focused && self.filtering {
            vec![theme::hints(&[("↑↓", "move"), ("enter", "open")]), theme::hints(&[("esc", "clear filter")])]
        } else if focused {
            vec![theme::hints(&[("↑↓", "move"), ("enter", "open"), ("/", "filter")]), theme::hints(&[("←", "wide"), ("^x", "stop"), ("esc", "hide")])]
        } else {
            vec![theme::hints(&[("←", "focus")])]
        };
        let room = height.saturating_sub(1 + keys.len());
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut at = 0;
        for (pos, e) in entries.into_iter().enumerate() {
            let on = pos == self.sel && focused;
            if pos == self.sel {
                at = lines.len();
            }
            match e {
                Entry::Head(_, t, style) => {
                    if !lines.is_empty() {
                        lines.push(Line::raw("")); // air between groups
                        if pos == self.sel {
                            at = lines.len();
                        }
                    }
                    lines.push(panel::row(on, head_spans(&t, style)));
                }
                Entry::Row(i) => {
                    let r = &self.rows[i];
                    let glyph = r.state.map_or(" ", State::glyph);
                    let title = if !r.title.is_empty() { &*r.title } else if !r.detail.is_empty() { &*r.detail } else { "(empty)" };
                    let text = fit(title, width.saturating_sub(8)).trim_end().to_string();
                    let dot = if r.tab { theme::CURRENT } else { " " };
                    let spans = if on { vec![Span::raw(format!("{glyph} {dot} {text}"))] } else { vec![Span::styled(glyph, Self::state_cell(r).style), Span::raw(format!(" {dot} {text}"))] };
                    lines.push(panel::row(on, spans));
                }
            }
        }
        if lines.is_empty() {
            lines.push(Line::styled(if self.loading { " loading…" } else { " no sessions yet" }, theme::dim()));
        }
        let first = if lines.len() <= room { 0 } else { at.saturating_sub(room / 2).min(lines.len() - room) };
        out.extend(lines.into_iter().skip(first).take(room));
        out.resize(height.saturating_sub(keys.len()).max(out.len()), Line::raw(""));
        out.extend(keys);
        out
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.entries().len().saturating_sub(1);
        let armed = self.armed.take();
        if ctrl && matches!(k.code, KeyCode::Char('f' | 't' | 'x')) {
            return self.act(k.code, armed);
        }
        if k.code == KeyCode::Delete || (ctrl && k.code == KeyCode::Char('k')) {
            return self.delete(armed);
        }
        match k.code {
            // Esc steps back: out of the filter first, then out of the list.
            KeyCode::Esc if self.filtering => self.clear_filter(),
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('c' | 'r' | 'd') if ctrl => return Outcome::Close,
            KeyCode::Right if self.cur().is_none() => return Outcome::Close,
            KeyCode::Enter | KeyCode::Right if k.code == KeyCode::Enter || self.cur().is_some() => {
                if self.cur().is_none() {
                    self.toggle_fold();
                } else if let Some(i) = self.cur() {
                    let r = &self.rows[i];
                    if r.state.is_some_and(State::live) && !r.tab {
                        let id = r.short.as_deref().unwrap_or(&r.id);
                        return Outcome::Refused(format!("running in the background: ctrl+f opens a copy, ctrl+t takes it over here (stops it), or `claude attach {id}`"));
                    }
                    let (id, title, cwd) = (r.id.clone(), r.title.to_string(), Some(r.cwd.to_string()).filter(|c| !c.is_empty()));
                    return if k.code == KeyCode::Right { Outcome::OpenHere(id, title, cwd) } else { Outcome::Open(id, title, cwd) };
                }
            }
            KeyCode::Tab => self.cycle_group(),
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(last),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = last,
            KeyCode::Char('/') if !ctrl && !self.filtering => self.filtering = true,
            KeyCode::Char('u') if ctrl && self.filtering => {
                self.query.clear();
                self.sel = self.first_row();
            }
            KeyCode::Backspace if self.filtering => {
                self.query.pop();
                self.sel = self.first_row();
            }
            KeyCode::Char(c) if !ctrl && self.filtering => {
                self.query.push(c);
                self.sel = self.first_row();
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// ctrl+f fork, ctrl+t take over, ctrl+x stop. Stopping a background session needs the same chord twice; an open tab's turn is just interrupted.
    fn act(&mut self, code: KeyCode, armed: Option<(String, char)>) -> Outcome {
        let Some(r) = self.cur().map(|i| &self.rows[i]) else { return Outcome::Stay };
        let KeyCode::Char(c) = code else { return Outcome::Stay };
        let target = || (r.id.clone(), r.title.to_string(), Some(r.cwd.to_string()).filter(|d| !d.is_empty()));
        if c == 'f' {
            let (id, title, cwd) = target();
            return Outcome::Fork(id, title, cwd);
        }
        if c == 'x' && r.tab && r.state.is_some_and(|s| matches!(s, State::Working | State::Blocked)) {
            return Outcome::Interrupt(r.id.clone());
        }
        let Some(short) = r.short.clone().filter(|_| r.state.is_some_and(State::live)) else {
            return Outcome::Refused("only a session running in the background can be stopped".into());
        };
        if armed != Some((r.id.clone(), c)) {
            self.armed = Some((r.id.clone(), c));
            let what = if c == 't' { "stops it and opens it here; its tool approvals do not carry over" } else { "stops it" };
            return Outcome::Refused(format!("press ctrl+{c} again to confirm: {what}"));
        }
        Outcome::Stop(short, (c == 't').then(target))
    }

    /// Delete / ctrl+k: remove the highlighted conversation from disk. Needs the same key twice.
    fn delete(&mut self, armed: Option<(String, char)>) -> Outcome {
        let Some(i) = self.cur() else { return Outcome::Stay };
        let r = &self.rows[i];
        if r.tab || r.state.is_some_and(State::live) {
            return Outcome::Refused("open or running: close or stop it before deleting".into());
        }
        let id = r.id.clone();
        if armed != Some((id.clone(), 'k')) {
            self.armed = Some((id, 'k'));
            return Outcome::Refused("press delete again to confirm: removes this conversation from disk for good".into());
        }
        if let Err(e) = history::delete(&id) {
            return Outcome::Refused(format!("not deleted: {e}"));
        }
        self.rows.remove(i);
        for p in [&mut self.preview, &mut self.peek] {
            if p.as_ref().is_some_and(|p| p.0 == id) {
                *p = None;
            }
        }
        self.sel = self.sel.min(self.entries().len().saturating_sub(1));
        Outcome::Refused("conversation deleted".into())
    }

    fn state_cell(r: &Row) -> Span<'static> {
        let style = match r.state {
            Some(State::Blocked) => theme::attention(),
            Some(State::Working) => theme::accent(),
            Some(State::Failed) => theme::danger(),
            _ => theme::dim(),
        };
        Span::styled(r.state.map_or(" ", State::glyph), style)
    }

    /// The panel for this explorer: a pinned filter box, the matching sessions, and (on wide screens) a
    /// preview of the end of the highlighted one. `width` is the terminal width, to wrap the preview.
    pub fn view(&mut self, width: usize) -> View {
        let m = self.matches();
        let entries = self.entries();
        self.sel = self.sel.min(entries.len().saturating_sub(1));
        let head = vec![
            if self.filtering {
                Line::from(vec![Span::styled("/ ", theme::accent()), Span::raw(self.query.clone()), Span::styled(" ", theme::selected())])
            } else {
                Line::styled("/ to filter", theme::dim())
            },
            Line::raw(""),
        ];
        // The list is 58 % of the screen when the preview sits beside it. Time, title and folder line up in columns.
        let main_w = if width >= panel::SIDE_MIN_WIDTH as usize { width.saturating_sub(2) * 58 / 100 } else { width.saturating_sub(2) };
        let dir_w = if main_w >= 64 { 16 } else { 0 };
        let title_w = main_w.saturating_sub(2 + 2 + 4 + 2 + if dir_w > 0 { dir_w + 2 } else { 0 }).max(10);
        let mut focus = 0;
        let body: Vec<Line> = if m.is_empty() {
            let msg = if self.loading { "loading…" } else if self.rows.is_empty() { "no sessions here yet" } else { "nothing matches" };
            vec![Line::styled(msg, theme::dim())]
        } else {
            let sel = self.sel;
            let mut lines: Vec<Line> = Vec::new();
            for (at, e) in entries.into_iter().enumerate() {
                match e {
                    Entry::Head(_, t, style) => {
                        if !lines.is_empty() {
                            lines.push(Line::raw("")); // air between groups
                        }
                        if at == sel {
                            focus = lines.len();
                        }
                        lines.push(panel::row_with(at == sel, false, head_spans(&t, style)));
                    }
                    Entry::Row(i) => {
                        if at == sel {
                            focus = lines.len();
                        }
                        let r = &self.rows[i];
                        let dot = if r.tab { theme::CURRENT } else { " " };
                        let title = if !r.title.is_empty() { &*r.title } else if !r.detail.is_empty() { &*r.detail } else { "(empty)" };
                        let mut spans = vec![Self::state_cell(r), Span::raw(dot), Span::styled(format!("{:>4}  ", history::ago(r.modified)), theme::dim()), Span::raw(fit(title, title_w))];
                        if dir_w > 0 {
                            let dir = std::path::Path::new(&*r.cwd).file_name().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                            spans.push(Span::styled(format!("  {}", fit(&dir, dir_w)), theme::dim()));
                        } else if let Some(dir) = std::path::Path::new(&*r.cwd).file_name() {
                            spans.push(Span::styled(format!("  {}", dir.to_string_lossy()), theme::dim()));
                        }
                        lines.push(panel::row(at == sel, spans));
                    }
                }
            }
            lines
        };
        let footer = theme::hints(&[("↑↓", "move"), ("enter", "open"), ("/", "filter"), ("tab", GROUPS[GROUP.load(Ordering::Relaxed) as usize]), ("^x", "stop"), ("del", "delete"), ("esc", "close")]);
        let title = format!("Sessions {}/{}", m.len(), self.rows.len());
        let mut v = View::new(title, body, focus, footer).with_head(head).rows(10, usize::MAX);
        if let Some(i) = self.cur() {
            let r = &self.rows[i];
            let (id, modified, size, state, detail) = (r.id.clone(), r.modified, r.size, r.state, r.detail.to_string());
            let side_w = (width.saturating_sub(4) * 42 / 100).saturating_sub(4);
            v = v.with_side(self.preview_lines(&id, modified, size, state, &detail, side_w));
        }
        v
    }

    fn preview_lines(&mut self, id: &str, modified: SystemTime, size: u64, state: Option<State>, detail: &str, width: usize) -> Vec<Line<'static>> {
        if self.preview.as_ref().map(|p| p.0.as_str()) != Some(id) {
            let turns = history::find(id).map(|p| history::preview(&p, PREVIEW_TURNS)).unwrap_or_default();
            self.preview = Some((id.to_string(), turns));
        }
        let mut lines = vec![Line::styled(format!("{} · {} ago · {}", &id[..id.len().min(8)], history::ago(modified), history::size(size)), theme::dim())];
        if state.is_some() && !detail.is_empty() {
            lines.extend(ui::wrap(detail, width).into_iter().map(|l| Line::styled(l, theme::bold())));
        }
        for (user, text) in &self.preview.as_ref().unwrap().1 {
            lines.push(Line::raw(""));
            let (mark, style) = if *user { ("❯ ", theme::accent()) } else { ("  ", Style::new()) };
            let wrapped = ui::wrap(text, width.saturating_sub(2));
            let more = wrapped.len() > PREVIEW_LINES_PER_TURN;
            for (i, l) in wrapped.into_iter().take(PREVIEW_LINES_PER_TURN).enumerate() {
                let tail = if more && i + 1 == PREVIEW_LINES_PER_TURN { " …" } else { "" };
                lines.push(Line::styled(format!("{}{l}{tail}", if i == 0 { mark } else { "  " }), style));
            }
        }
        if lines.len() == 1 {
            lines.push(Line::styled("(no readable messages)", theme::dim()));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    fn sum(id: &str, title: &str, prompt: &str) -> Summary {
        Summary { id: id.into(), title: title.into(), prompt: prompt.into(), modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000), size: 2048, cwd: String::new() }
    }
    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn explorer() -> Explorer {
        Explorer::from(
            "/p",
            vec![sum("aaa111", "Fix login bug", "the login form 500s"), sum("bbb222", "Refactor parser", "split the parser module"), sum("ccc333", "Login tests", "add tests")],
            vec![],
            vec![("bbb222".into(), State::Done)],
        )
    }

    #[test]
    fn delete_needs_the_key_twice_and_refuses_open_tabs() {
        let mut e = explorer();
        e.seek(Some("bbb222".into())); // open in a tab
        assert!(matches!(e.key(key(KeyCode::Delete)), Outcome::Refused(m) if m.contains("close or stop")));
        e.seek(Some("aaa111".into()));
        assert!(matches!(e.key(key(KeyCode::Delete)), Outcome::Refused(m) if m.contains("again to confirm")));
        assert_eq!(e.armed, Some(("aaa111".into(), 'k')));
        e.key(key(KeyCode::Down)); // any other key disarms
        assert!(e.armed.is_none());
    }

    #[test]
    fn filter_needs_every_term_and_searches_title_prompt_and_id() {
        let mut e = explorer();
        assert_eq!(e.matches(), vec![1, 0, 2], "the open (done) tab sorts above earlier sessions");
        e.query = "login".into();
        assert_eq!(e.matches(), vec![0, 2]);
        e.query = "LOGIN tests".into();
        assert_eq!(e.matches(), vec![2]);
        e.query = "parser module".into(); // second term only in the first prompt
        assert_eq!(e.matches(), vec![1]);
        e.query = "bbb".into();
        assert_eq!(e.matches(), vec![1]);
    }

    #[test]
    fn keys_select_filter_open_and_close() {
        let mut e = explorer();
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Open(id, t, None) if id == "bbb222" && t == "Refactor parser"));
        for c in "log".chars() {
            e.key(key(KeyCode::Char(c)));
        }
        assert!(e.query.is_empty(), "letters alone start nothing");
        for c in "/log".chars() {
            e.key(key(KeyCode::Char(c)));
        }
        assert_eq!(e.selected().map(|x| x.0), Some("aaa111".into()), "/ then typing puts the cursor on the first match");
        e.key(key(KeyCode::Down));
        e.key(key(KeyCode::Down)); // clamped to the last match
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Open(id, ..) if id == "ccc333"));
        e.key(key(KeyCode::Char('x')));
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Stay), "no match, nothing to open");
        assert!(matches!(e.key(key(KeyCode::Esc)), Outcome::Stay), "esc leaves the filter first");
        assert!(e.query.is_empty() && !e.filtering);
        assert!(matches!(e.key(key(KeyCode::Esc)), Outcome::Close));
    }

    #[test]
    fn groups_open_tabs_by_their_state_and_forgets_it_when_the_tab_closes() {
        let mut e = explorer();
        e.set_open(vec![("aaa111".into(), State::Blocked), ("bbb222".into(), State::Working)]);
        let heads: Vec<String> = e.entries().into_iter().filter_map(|x| if let Entry::Head(_, t, _) = x { Some(t) } else { None }).collect();
        assert_eq!(heads, vec!["▾ ◆ needs you 1", "▾ ◐ working 1", "▾ ○ earlier 1"]);
        e.sel = 1;
        assert_eq!(e.selected(), Some(("aaa111".into(), true)), "the one that needs you is first (row 0 is its heading)");
        e.sel = 0;
        assert_eq!(e.selected(), None, "a heading is not a session");
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Stay));
        assert_eq!(e.entries().len(), 5, "the folded group keeps its heading, loses its row");
        e.key(key(KeyCode::Enter));
        assert_eq!(e.entries().len(), 6, "Enter again unfolds it");
        e.set_open(vec![]);
        e.sel = 1;
        assert_eq!(e.selected().map(|x| x.1), Some(false));
        assert!(e.rows.iter().all(|r| r.state.is_none() && !r.tab));
    }

    fn job(session: &str, state: State, cwd: &str, name: &str, detail: &str) -> Job {
        Job { short: session[..3].into(), session: session.into(), state, cwd: cwd.into(), name: name.into(), detail: detail.into(), modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000) }
    }

    #[test]
    fn background_sessions_join_the_list_and_those_that_want_you_come_first() {
        let jobs = vec![
            job("aaa111", State::Done, "/p", "Fix login bug (renamed)", "shipped"),
            job("zzz999", State::Blocked, "/other/repo", "deploy", "pick A or B"),
            job("yyy888", State::Working, "/p", "", "running tests"),
        ];
        let mut e = Explorer::from("/p", vec![sum("aaa111", "Fix login bug", "p"), sum("bbb222", "Refactor parser", "p")], jobs, vec![]);
        let ids: Vec<&str> = e.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids[..2], ["zzz999", "yyy888"], "blocked, then working, then the rest by recency: {ids:?}");
        assert_eq!(e.rows.len(), 4, "the done job merged into its transcript");
        let merged = e.rows.iter().find(|r| r.id == "aaa111").unwrap();
        assert_eq!((&*merged.title, &*merged.detail, merged.size), ("Fix login bug (renamed)", "shipped", 2048));
        assert_eq!(&*e.rows[0].cwd, "/other/repo");
        assert_eq!(&*e.rows[1].cwd, "", "our own directory is not repeated");
        e.query = "s:needs".into();
        assert_eq!(e.matches().len(), 1);
        e.query = "s:work pick".into();
        assert_eq!(e.matches().len(), 0, "terms are ANDed");
        e.query = "other/repo".into();
        assert_eq!(e.matches().len(), 1, "directory is searchable");
        e.query.clear();
        // a live session cannot be resumed headless: explain instead of opening
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Refused(m) if m.contains("ctrl+t takes it over")));
        e.sel = e.entries().iter().position(|x| matches!(x, Entry::Row(i) if e.rows[*i].id == "aaa111")).unwrap();
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Open(id, ..) if id == "aaa111"));
    }

    #[test]
    fn take_over_and_stop_need_the_same_chord_twice_and_only_work_on_live_sessions() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let mut e = Explorer::from("/p", vec![sum("aaa111", "idle one", "p")], vec![job("zzz999", State::Blocked, "/o", "deploy", "q")], vec![]);
        assert!(matches!(e.key(ctrl('t')), Outcome::Refused(m) if m.contains("again to confirm")));
        e.key(key(KeyCode::Down));
        e.key(key(KeyCode::Down)); // past the next heading
        assert!(matches!(e.key(ctrl('t')), Outcome::Refused(m) if m.contains("only a session running")), "an idle one cannot be stopped");
        e.key(key(KeyCode::Up));
        e.key(key(KeyCode::Up));
        e.key(ctrl('x'));
        assert!(matches!(e.key(ctrl('t')), Outcome::Refused(_)), "a different chord re-arms instead of confirming");
        assert!(matches!(e.key(ctrl('t')), Outcome::Stop(s, Some((id, _, Some(d)))) if s == "zzz" && id == "zzz999" && d == "/o"));
        e.key(ctrl('x'));
        assert!(matches!(e.key(ctrl('x')), Outcome::Stop(_, None)));
        assert!(matches!(e.key(ctrl('f')), Outcome::Fork(id, ..) if id == "zzz999"));
    }

    #[test]
    fn ctrl_x_interrupts_an_open_tab_that_is_working_without_asking_twice() {
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let mut e = explorer();
        e.set_open(vec![("aaa111".into(), State::Working)]);
        e.seek(Some("aaa111".into()));
        assert!(matches!(e.key(ctrl_x), Outcome::Interrupt(id) if id == "aaa111"));
        e.set_open(vec![("aaa111".into(), State::Done)]);
        assert!(matches!(e.key(ctrl_x), Outcome::Refused(_)), "an idle tab has nothing to stop");
    }

    #[test]
    fn draws_list_marks_open_sessions_and_shows_a_preview_when_wide() {
        let mut e = explorer();
        for width in [60u16, 120] {
            let mut t = Terminal::new(TestBackend::new(width, 20)).unwrap();
            t.draw(|f| {
                let v = e.view(width as usize);
                let h = panel::height(&v, 20);
                panel::render(f, ratatui::layout::Rect::new(0, 20 - h, width, h), v);
            })
            .unwrap();
            let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
            assert!(text.contains("Fix login bug") && text.contains("Sessions 3/3"), "{width}");
            assert!(text.contains('●'), "open session is marked");
            assert_eq!(text.contains("split the parser module"), width >= 90, "preview pane only when wide");
        }
        e.query = "zzz".into();
        let v = e.view(60);
        assert_eq!(v.title, "Sessions 0/3");
        assert!(v.body[0].spans[0].content.contains("nothing matches"));
    }
}

#[cfg(test)]
mod real_data {
    /// Opens the list against this machine's real files (run with `--ignored --nocapture`).
    #[test]
    #[ignore]
    fn real_list_cost() {
        let t = std::time::Instant::now();
        let e = super::Explorer::new(&std::env::current_dir().unwrap().to_string_lossy(), vec![]);
        let live = e.rows.iter().filter(|r| r.state.is_some_and(|s| s.live())).count();
        let bytes: usize = e.rows.iter().map(|r| std::mem::size_of::<super::Row>() + r.id.len() + r.title.len() + r.detail.len() + r.cwd.len()).sum();
        println!("rows={} live={live} built in {:?}, ~{} KB held", e.rows.len(), t.elapsed(), bytes / 1024);
    }
}
