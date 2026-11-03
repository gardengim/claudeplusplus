//! `/schedule`: pick when. A short list of times with the cursor on one, or type your own (`45m`, `2h30m`,
//! `17:30`). The message is then typed in the ordinary message box (or, from the queue panel, it is the queued
//! message that gets the time). The keys only move and edit; the app acts on what they ask.

use crate::panel::{self, View};
use crate::queue::{clock, parse_when, until};
use crate::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use std::time::SystemTime;

pub struct Ui {
    pub cursor: usize,
    /// A time typed instead of picked.
    pub custom: String,
    /// A queued message (its row) that gets the time, instead of a new one being typed.
    pub target: Option<usize>,
    /// Opened from the queue panel: go back there when done.
    pub from_queue: bool,
    /// The message given with the command, for when its time was missing or wrong.
    pub msg: Option<String>,
}

pub enum Act {
    None,
    Close,
    /// Use this time; `None` takes the time off a queued message.
    Pick(Option<SystemTime>),
    /// What was typed is not a time.
    Bad,
}

/// Times offered, each with what it says. `unset` adds "no time" for a message that has one.
fn choices(now: SystemTime, unset: bool) -> Vec<(String, Option<SystemTime>)> {
    let at = |w: &str| parse_when(w, now);
    let mut v = vec![];
    if unset {
        v.push(("no time: goes when the turn ends".to_string(), None));
    }
    for (label, w) in [("in 5 minutes", "5m"), ("in 15 minutes", "15m"), ("in 30 minutes", "30m"), ("in 1 hour", "1h"), ("in 2 hours", "2h"), ("in 4 hours", "4h"), ("next 9:00", "9:00"), ("next 18:00", "18:00")] {
        v.push((label.to_string(), at(w)));
    }
    v
}

impl Ui {
    pub fn new(custom: &str) -> Self {
        Ui { cursor: 0, custom: custom.into(), target: None, from_queue: false, msg: None }
    }

    pub fn key(&mut self, k: KeyEvent, now: SystemTime, unset: bool) -> Act {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let list = choices(now, unset);
        self.cursor = self.cursor.min(list.len() - 1);
        match k.code {
            KeyCode::Esc => return Act::Close,
            KeyCode::Char('c') if ctrl => return Act::Close,
            KeyCode::Up => self.cursor = panel::cycle(self.cursor, list.len(), true),
            KeyCode::Down | KeyCode::Tab => self.cursor = panel::cycle(self.cursor, list.len(), false),
            KeyCode::Enter if self.custom.trim().is_empty() => return Act::Pick(list[self.cursor].1),
            KeyCode::Enter => return parse_when(self.custom.trim(), now).map_or(Act::Bad, |t| Act::Pick(Some(t))),
            KeyCode::Char('u') if ctrl => self.custom.clear(),
            KeyCode::Backspace => _ = self.custom.pop(),
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => self.custom.push(c),
            _ => {}
        }
        Act::None
    }
}

pub fn view(ui: &Ui, queued: Option<&str>, unset: bool, now: SystemTime) -> View {
    let typed = !ui.custom.trim().is_empty();
    let mut head = vec![Span::styled("type a time ", theme::dim()), Span::raw(ui.custom.clone()), Span::styled(" ", theme::selected())];
    if !typed {
        head.push(Span::styled("  45m · 2h30m · 17:30", theme::dim()));
    }
    let mut head = vec![Line::from(head)];
    if let Some(q) = queued.or(ui.msg.as_deref()) {
        head.insert(0, Line::styled(format!("message: {q}"), theme::dim()));
    }
    let list = choices(now, unset);
    let cursor = ui.cursor.min(list.len() - 1);
    let body = if typed {
        let line = match parse_when(ui.custom.trim(), now) {
            Some(t) => Line::from(vec![Span::raw(format!("sends at {} (in {})", clock(t), until(t, now)))]),
            None => Line::styled("not a time: try 45m, 2h30m or 17:30", theme::dim()),
        };
        vec![panel::row(true, line.spans)]
    } else {
        list.iter()
            .enumerate()
            .map(|(i, (label, t))| {
                let when = t.map_or(String::new(), |t| format!("  {}", clock(t)));
                panel::row(i == cursor, vec![Span::raw(format!("{label:<34}")), Span::styled(when, theme::dim())])
            })
            .collect()
    };
    let enter = if typed { "use it" } else { "pick" };
    View::new("Schedule", body, if typed { 0 } else { cursor }, theme::hints(&[("↑↓", "move"), ("enter", enter), ("type", "your own time"), ("esc", "close")])).with_head(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn enter_picks_the_row_or_the_typed_time_and_a_bad_one_is_refused() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut ui = Ui::new("");
        let key = |ui: &mut Ui, c| ui.key(KeyEvent::new(c, KeyModifiers::NONE), now, false);
        assert!(matches!(key(&mut ui, KeyCode::Enter), Act::Pick(Some(t)) if t == now + Duration::from_secs(300)));
        key(&mut ui, KeyCode::Down);
        assert!(matches!(key(&mut ui, KeyCode::Enter), Act::Pick(Some(t)) if t == now + Duration::from_secs(900)));
        "7x".chars().for_each(|c| _ = key(&mut ui, KeyCode::Char(c)));
        assert!(matches!(key(&mut ui, KeyCode::Enter), Act::Bad));
        key(&mut ui, KeyCode::Backspace);
        key(&mut ui, KeyCode::Char('m'));
        assert!(matches!(key(&mut ui, KeyCode::Enter), Act::Pick(Some(t)) if t == now + Duration::from_secs(420)));
        assert!(matches!(key(&mut ui, KeyCode::Esc), Act::Close));
    }

    #[test]
    fn a_queued_message_can_be_taken_off_its_time() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut ui = Ui::new("");
        assert!(matches!(ui.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), now, true), Act::Pick(None)));
    }
}
