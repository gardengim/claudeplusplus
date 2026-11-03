//! Messages that wait for the running turn to end, or (`/schedule`) for a time of their own: a strip above the
//! input says they are there, and `/queue` (alt+q) reorders, edits, deletes or sends them. The keys only change
//! the list; the app acts on what they ask.

use crate::panel::{self, View, cycle};
use crate::session::Queued;
use crate::theme;
use crate::ui::wrap;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Hour, minute and second of the local day at `t`.
fn local_hms(t: SystemTime) -> (u64, u64, u64) {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: localtime_r only writes the tm it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    (tm.tm_hour as u64, tm.tm_min as u64, tm.tm_sec as u64)
}

/// How long from `now` until `t`, as `45s`, `9m` or `2h 5m`.
pub fn until(t: SystemTime, now: SystemTime) -> String {
    let s = t.duration_since(now).map_or(0, |d| d.as_secs());
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// The local time of day, `14:30`.
pub fn clock(t: SystemTime) -> String {
    let (h, m, _) = local_hms(t);
    format!("{h:02}:{m:02}")
}

/// When `/schedule` should send: after `10m`, `90s`, `2h30m`, or at the next `14:30` (24-hour, local).
/// ponytail: a clock time is "seconds until then" from now, so a daylight-saving change in between shifts it by an hour.
pub fn parse_when(w: &str, now: SystemTime) -> Option<SystemTime> {
    if let Some((h, m)) = w.split_once(':') {
        let (h, m): (u64, u64) = (h.parse().ok()?, m.parse().ok()?);
        if h > 23 || m > 59 {
            return None;
        }
        let (nh, nm, ns) = local_hms(now);
        let wait = (h * 3600 + m * 60 + 86400 - (nh * 3600 + nm * 60 + ns)) % 86400;
        return Some(now + Duration::from_secs(if wait == 0 { 86400 } else { wait }));
    }
    let (mut total, mut num) = (0u64, String::new());
    for c in w.chars() {
        match c {
            '0'..='9' => num.push(c),
            'h' | 'm' | 's' if !num.is_empty() => {
                total += num.parse::<u64>().ok()? * match c { 'h' => 3600, 'm' => 60, _ => 1 };
                num.clear();
            }
            _ => return None,
        }
    }
    (num.is_empty() && total > 0).then(|| now + Duration::from_secs(total))
}

#[derive(Default)]
pub struct Ui {
    pub cursor: usize,
}

pub enum Act {
    None,
    Close,
    /// Interrupt the running turn and send this one next.
    Send(usize),
    /// Send it now without interrupting: Claude Code takes it in at its next step.
    Steer(usize),
    /// Take it back into the message box.
    Edit(usize),
    Remove(usize),
    Clear,
    Pause,
    /// Give it a time (`S`).
    Schedule(usize),
}

/// The message on one row: its first line, and how many more it has.
pub fn gist(q: &Queued, width: usize) -> String {
    let mut lines = q.text.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next().unwrap_or("").trim();
    let more = lines.count();
    let tail = if more > 0 { format!(" (+{more} lines)") } else { String::new() };
    let text: String = first.chars().take(width.saturating_sub(tail.chars().count())).collect();
    format!("{text}{}{tail}", if first.chars().count() > text.chars().count() { "…" } else { "" })
}

impl Ui {
    pub fn key(&mut self, k: KeyEvent, q: &mut [Queued]) -> Act {
        let n = q.len();
        let (ctrl, alt, shift) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT), k.modifiers.contains(KeyModifiers::SHIFT));
        self.cursor = self.cursor.min(n.saturating_sub(1));
        let at = self.cursor;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Act::Close,
            KeyCode::Char('c') if ctrl => return Act::Close,
            _ if n == 0 => {}
            // Moving a message: it keeps the cursor.
            KeyCode::Up | KeyCode::Down if alt || shift => {
                let to = if k.code == KeyCode::Up { at.checked_sub(1) } else { Some(at + 1).filter(|t| *t < n) };
                if let Some(to) = to {
                    q.swap(at, to);
                    self.cursor = to;
                }
            }
            KeyCode::Char('K') => return self.key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), q),
            KeyCode::Char('J') => return self.key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), q),
            KeyCode::Char('t') => {
                q[..=at].rotate_right(1);
                self.cursor = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => self.cursor = cycle(at, n, true),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = cycle(at, n, false),
            KeyCode::Enter => return Act::Send(at),
            KeyCode::Char('s') => return Act::Steer(at),
            KeyCode::Char('e') => return Act::Edit(at),
            KeyCode::Char('d' | 'x') | KeyCode::Delete | KeyCode::Backspace => return Act::Remove(at),
            KeyCode::Char('C') => return Act::Clear,
            KeyCode::Char('p') => return Act::Pause,
            KeyCode::Char('S') => return Act::Schedule(at),
            _ => {}
        }
        Act::None
    }
}

pub fn view(ui: &Ui, q: &[Queued], paused: bool, busy: bool, width: usize) -> View {
    let timed = q.iter().filter(|m| m.due.is_some()).count();
    let state = if paused { " · paused" } else if busy && timed < q.len() { " · each goes when the turn ends" } else { "" };
    let timed_note = if timed > 0 { format!(", {timed} scheduled") } else { String::new() };
    if q.is_empty() {
        return View::new("Queue", vec![Line::styled("nothing is waiting; a message sent while Claude works waits here, /schedule <when> <message> sends one later", theme::dim())], 0, theme::hints(&[("esc", "close")]));
    }
    let cursor = ui.cursor.min(q.len() - 1);
    let body = q
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let pics = if m.atts.is_empty() { String::new() } else { format!("  ▣ {}", m.atts.len()) };
            let at = m.due.map_or(String::new(), |d| format!("@{}  ", clock(d)));
            panel::row(i == cursor, vec![Span::styled(format!("{:>2}  ", i + 1), theme::dim()), Span::styled(at.clone(), theme::accent()), Span::raw(gist(m, width.saturating_sub(12 + at.len()))), Span::styled(pics, theme::dim())])
        })
        .collect();
    let footer = theme::hints(&[("↑↓", "move"), ("alt+↑↓", "reorder"), ("enter", "send now"), ("s", "steer"), ("e", "edit"), ("d", "delete"), ("S", "schedule"), ("p", "pause"), ("esc", "close")]);
    let side = wrap(&q[cursor].text, (width / 2).saturating_sub(4).max(10)).into_iter().map(Line::raw).collect();
    View::new(format!("Queue · {} waiting{timed_note}{state}", q.len()), body, cursor, footer).with_side(side)
}

/// The strip above the input: the first few messages, and the way in.
pub fn strip(q: &[Queued], paused: bool, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = q
        .iter()
        .take(3)
        .enumerate()
        .map(|(i, m)| {
            let lead = if i == 0 { if paused { "⏸ " } else { "⧗ " } } else { "  " };
            let n = m.due.map_or((i + 1).to_string(), |d| format!("@{}", clock(d)));
            Line::from(vec![Span::styled(format!("{lead}{n}  "), theme::dim()), Span::styled(gist(m, width.saturating_sub(40).max(10)), theme::dim())])
        })
        .collect();
    let more = q.len().saturating_sub(3);
    let hint = theme::keys(&format!("{}alt+q to manage", if more > 0 { format!("+{more} more · ") } else { String::new() }));
    if let Some(last) = lines.last_mut() {
        let used: usize = last.spans.iter().map(Span::width).sum();
        let pad = width.saturating_sub(used + hint.chars().count() + 1);
        last.spans.push(Span::raw(" ".repeat(pad)));
        last.spans.push(Span::styled(hint, theme::dim()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_is_a_delay_or_the_next_clock_time() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let after = |w| parse_when(w, now).map(|t| t.duration_since(now).unwrap().as_secs());
        assert_eq!((after("10m"), after("90s"), after("2h30m")), (Some(600), Some(90), Some(9000)));
        assert_eq!((after(""), after("10"), after("m"), after("0m"), after("5x"), after("25:00"), after("9:61")), (None, None, None, None, None, None, None));
        let (h, m, _) = local_hms(now);
        let ahead = after(&format!("{h}:{m}")).unwrap();
        assert!(ahead > 0 && ahead <= 86400, "this minute is the next time it is {h}:{m:02}: {ahead}");
        assert_eq!(clock(parse_when(&format!("{h}:{m:02}"), now).unwrap()), format!("{h:02}:{m:02}"));
    }
}
