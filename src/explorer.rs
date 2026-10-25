//! Full-screen session list: this directory's earlier conversations (Claude Code's transcripts, see
//! history.rs) plus the supervisor's background sessions from every directory (see jobs.rs). Rows that
//! want you come first. Type to filter (`s:work` filters by state), see the end of the highlighted
//! conversation, Enter to open it. Built when opened and dropped when closed.

use crate::history::{self, Summary};
use crate::jobs::{self, Job, State};
use crate::panel::{self, View};
use crate::{theme, ui};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::time::SystemTime;

const MAX_SESSIONS: usize = 200;
const PREVIEW_TURNS: usize = 8;
const PREVIEW_LINES_PER_TURN: usize = 6;

pub enum Outcome {
    Stay,
    Close,
    /// Session id, its title and the directory it lives in (`None` = ours).
    Open(String, String, Option<String>),
    /// Open a copy (`--fork-session`) so the original is left alone.
    Fork(String, String, Option<String>),
    /// `claude stop <short>`; then open the session here if given.
    Stop(String, Option<(String, String, Option<String>)>),
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
    modified: SystemTime,
    size: u64,
}

pub struct Explorer {
    rows: Vec<Row>,
    /// Session ids already open in a tab.
    open: Vec<String>,
    pub query: String,
    sel: usize,
    preview: Option<(String, Vec<(bool, String)>)>,
    /// A destructive chord pressed once: (session id, which); the same chord again confirms.
    armed: Option<(String, char)>,
}

fn merge(here: &str, items: Vec<Summary>, jobs: Vec<Job>) -> Vec<Row> {
    let mut rows: Vec<Row> = items
        .into_iter()
        .map(|s| Row { id: s.id, short: None, title: s.title.into(), detail: s.prompt.into(), cwd: "".into(), state: None, modified: s.modified, size: s.size })
        .collect();
    for j in jobs {
        let cwd: Box<str> = if &*j.cwd == here { "".into() } else { j.cwd };
        match rows.iter_mut().find(|r| r.id == j.session) {
            Some(r) => {
                if !j.name.is_empty() {
                    r.title = j.name;
                }
                (r.short, r.detail, r.state, r.cwd) = (Some(j.short), j.detail, Some(j.state), cwd);
                r.modified = r.modified.max(j.modified);
            }
            None => rows.push(Row { id: j.session, short: Some(j.short), title: j.name, detail: j.detail, cwd, state: Some(j.state), modified: j.modified, size: 0 }),
        }
    }
    // Sessions that want you, then the rest by recency.
    rows.sort_by_key(|r| (r.state.filter(|s| s.live()).map_or(2, |s| s as u8), std::cmp::Reverse(r.modified)));
    rows.truncate(MAX_SESSIONS);
    rows
}

impl Explorer {
    pub fn new(cwd: &str, open: Vec<String>) -> Self {
        Self::from(cwd, history::recent(cwd, MAX_SESSIONS), jobs::load(), open)
    }

    #[cfg(test)]
    pub fn for_bench(items: Vec<Summary>) -> Self {
        Self::from("", items, vec![], vec![])
    }

    fn from(here: &str, items: Vec<Summary>, jobs: Vec<Job>, open: Vec<String>) -> Self {
        Explorer { rows: merge(here, items, jobs), open, query: String::new(), sel: 0, preview: None, armed: None }
    }

    /// Indices of rows matching every space-separated term of the filter; `s:<state>` terms match the
    /// state label, the rest title, summary, directory and id.
    fn matches(&self) -> Vec<usize> {
        let q = self.query.to_lowercase();
        let terms: Vec<&str> = q.split_whitespace().collect();
        (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                let hay = format!("{} {} {} {}", r.title, r.detail, r.cwd, r.id).to_lowercase();
                terms.iter().all(|t| match t.strip_prefix("s:") {
                    Some(st) => r.state.is_some_and(|s| s.label().starts_with(st) || (st == "needs" && s == State::Blocked)),
                    None => hay.contains(t),
                })
            })
            .collect()
    }

    /// Re-read the sources, keeping the cursor on the same session.
    pub fn refresh(&mut self, here: &str, open: Vec<String>) {
        let keep = self.matches().get(self.sel).map(|&i| self.rows[i].id.clone());
        self.rows = merge(here, history::recent(here, MAX_SESSIONS), jobs::load());
        self.open = open;
        if let Some(p) = keep.and_then(|id| self.matches().iter().position(|&i| self.rows[i].id == id)) {
            self.sel = p;
        }
    }

    /// The docked column: one compact line per session, only the rows that fit around the cursor.
    pub fn dock_lines(&mut self, width: usize, height: usize, focused: bool) -> Vec<Line<'static>> {
        let m = self.matches();
        self.sel = self.sel.min(m.len().saturating_sub(1));
        let mut out = vec![Line::styled(" Sessions", theme::dim())];
        let room = height.saturating_sub(1);
        let first = if m.len() <= room { 0 } else { self.sel.saturating_sub(room / 2).min(m.len() - room) };
        for (k, &i) in m.iter().enumerate().skip(first).take(room) {
            let r = &self.rows[i];
            let glyph = r.state.map_or(" ", State::glyph);
            let title = if !r.title.is_empty() { &*r.title } else if !r.detail.is_empty() { &*r.detail } else { "(empty)" };
            let text: String = title.chars().take(width.saturating_sub(6)).collect();
            let dot = if self.open.contains(&r.id) { theme::CURRENT } else { " " };
            let mut spans = vec![Span::styled(glyph, Self::state_cell(r).style), Span::raw(format!("{dot}{text}"))];
            if k == self.sel && focused {
                spans = vec![Span::raw(format!("{glyph}{dot}{text}"))];
            }
            out.push(panel::row(k == self.sel && focused, spans));
        }
        out
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.matches().len().saturating_sub(1);
        let armed = self.armed.take();
        if ctrl && matches!(k.code, KeyCode::Char('f' | 't' | 'x')) {
            return self.act(k.code, armed);
        }
        match k.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('c' | 'r' | 'd') if ctrl => return Outcome::Close,
            KeyCode::Right if self.query.is_empty() => return Outcome::Close,
            KeyCode::Enter => {
                if let Some(&i) = self.matches().get(self.sel) {
                    let r = &self.rows[i];
                    if r.state.is_some_and(State::live) {
                        let id = r.short.as_deref().unwrap_or(&r.id);
                        return Outcome::Refused(format!("running in the background: ctrl+f opens a copy, ctrl+t takes it over here (stops it), or `claude attach {id}`"));
                    }
                    return Outcome::Open(r.id.clone(), r.title.to_string(), Some(r.cwd.to_string()).filter(|c| !c.is_empty()));
                }
            }
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(last),
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(last),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = last,
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.sel = 0;
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.sel = 0;
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// ctrl+f fork, ctrl+t take over, ctrl+x stop. Stopping needs the same chord twice.
    fn act(&mut self, code: KeyCode, armed: Option<(String, char)>) -> Outcome {
        let Some(r) = self.matches().get(self.sel).map(|&i| &self.rows[i]) else { return Outcome::Stay };
        let KeyCode::Char(c) = code else { return Outcome::Stay };
        let target = || (r.id.clone(), r.title.to_string(), Some(r.cwd.to_string()).filter(|d| !d.is_empty()));
        if c == 'f' {
            let (id, title, cwd) = target();
            return Outcome::Fork(id, title, cwd);
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

    fn state_cell(r: &Row) -> Span<'static> {
        let style = match r.state {
            Some(State::Blocked) => theme::attention(),
            Some(State::Working) => theme::accent(),
            Some(State::Failed) => theme::danger(),
            _ => theme::dim(),
        };
        Span::styled(format!("{} ", r.state.map_or(" ", State::glyph)), style)
    }

    /// The panel for this explorer: a pinned filter box, the matching sessions, and (on wide screens) a
    /// preview of the end of the highlighted one. `width` is the terminal width, to wrap the preview.
    pub fn view(&mut self, width: usize) -> View {
        let m = self.matches();
        self.sel = self.sel.min(m.len().saturating_sub(1));
        let head = vec![Line::from(vec![Span::styled("filter ", theme::dim()), Span::raw(self.query.clone()), Span::styled(" ", theme::selected())])];
        let body: Vec<Line> = if m.is_empty() {
            let msg = if self.rows.is_empty() { "no sessions here yet" } else { "nothing matches" };
            vec![Line::styled(msg, theme::dim())]
        } else {
            m.iter()
                .enumerate()
                .map(|(row_i, &i)| {
                    let r = &self.rows[i];
                    let dot = if self.open.contains(&r.id) { theme::CURRENT } else { " " };
                    let title = if !r.title.is_empty() { &*r.title } else if !r.detail.is_empty() { &*r.detail } else { "(empty)" };
                    let mut spans = vec![Span::raw(format!("{dot} ")), Self::state_cell(r), Span::raw(format!("{:>4}  {title}", history::ago(r.modified)))];
                    if let Some(dir) = std::path::Path::new(&*r.cwd).file_name() {
                        spans.push(Span::styled(format!("  {}", dir.to_string_lossy()), theme::dim()));
                    }
                    panel::row(row_i == self.sel, spans)
                })
                .collect()
        };
        let footer = theme::hints(&[("↑↓", "move"), ("enter", "open"), ("type", "to filter"), ("s:work", "by state"), ("ctrl+f", "fork"), ("ctrl+t", "take over"), ("esc/→", "close")]);
        let title = format!("Sessions {}/{}", m.len(), self.rows.len());
        let mut v = View::new(title, body, self.sel, footer).with_head(head).rows(10, usize::MAX);
        if let Some(&i) = m.get(self.sel) {
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
            let (mark, style) = if *user { ("> ", theme::accent()) } else { ("  ", Style::new()) };
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
        Summary { id: id.into(), title: title.into(), prompt: prompt.into(), modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000), size: 2048 }
    }
    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn explorer() -> Explorer {
        Explorer::from(
            "/p",
            vec![sum("aaa111", "Fix login bug", "the login form 500s"), sum("bbb222", "Refactor parser", "split the parser module"), sum("ccc333", "Login tests", "add tests")],
            vec![],
            vec!["bbb222".into()],
        )
    }

    #[test]
    fn filter_needs_every_term_and_searches_title_prompt_and_id() {
        let mut e = explorer();
        assert_eq!(e.matches(), vec![0, 1, 2]);
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
        e.key(key(KeyCode::Down));
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Open(id, t, None) if id == "bbb222" && t == "Refactor parser"));
        for c in "log".chars() {
            e.key(key(KeyCode::Char(c)));
        }
        assert_eq!(e.sel, 0, "typing resets the selection");
        e.key(key(KeyCode::Down));
        e.key(key(KeyCode::Down)); // clamped to the last match
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Open(id, ..) if id == "ccc333"));
        e.key(key(KeyCode::Char('x')));
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Stay), "no match, nothing to open");
        assert!(matches!(e.key(key(KeyCode::Esc)), Outcome::Close));
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
        e.key(key(KeyCode::Down));
        e.key(key(KeyCode::Down));
        assert!(matches!(e.key(key(KeyCode::Enter)), Outcome::Open(id, ..) if id == "aaa111" || id == "bbb222"));
    }

    #[test]
    fn take_over_and_stop_need_the_same_chord_twice_and_only_work_on_live_sessions() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let mut e = Explorer::from("/p", vec![sum("aaa111", "idle one", "p")], vec![job("zzz999", State::Blocked, "/o", "deploy", "q")], vec![]);
        assert!(matches!(e.key(ctrl('t')), Outcome::Refused(m) if m.contains("again to confirm")));
        e.key(key(KeyCode::Down));
        assert!(matches!(e.key(ctrl('t')), Outcome::Refused(m) if m.contains("only a session running")), "an idle one cannot be stopped");
        e.key(key(KeyCode::Up));
        e.key(ctrl('x'));
        assert!(matches!(e.key(ctrl('t')), Outcome::Refused(_)), "a different chord re-arms instead of confirming");
        assert!(matches!(e.key(ctrl('t')), Outcome::Stop(s, Some((id, _, Some(d)))) if s == "zzz" && id == "zzz999" && d == "/o"));
        e.key(ctrl('x'));
        assert!(matches!(e.key(ctrl('x')), Outcome::Stop(_, None)));
        assert!(matches!(e.key(ctrl('f')), Outcome::Fork(id, ..) if id == "zzz999"));
    }

    #[test]
    fn draws_list_marks_open_sessions_and_shows_a_preview_when_wide() {
        let mut e = explorer();
        for width in [60u16, 120] {
            let mut t = Terminal::new(TestBackend::new(width, 20)).unwrap();
            t.draw(|f| {
                let v = e.view(width as usize);
                let h = panel::height(&v, 20);
                panel::render(f, ratatui::layout::Rect::new(0, 20 - h, width, h), v)
            })
            .unwrap();
            let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
            assert!(text.contains("Fix login bug") && text.contains("Sessions 3/3"), "{width}");
            assert!(text.contains('●'), "open session is marked");
            assert_eq!(text.contains("(no readable messages)"), width >= 90, "preview pane only when wide");
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
