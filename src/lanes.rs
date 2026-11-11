//! `/tasks`: this tab's subagents and shell commands Claude Code runs as tasks, as a list (state, what each was asked), and for one of them its steps and
//! report. Everything is read from the transcript on every draw, so a running agent updates live and there
//! is no state of its own to go stale. Nothing here talks to Claude Code or costs tokens.

use crate::jobs::State;
use crate::panel::{self, View, cycle};
use crate::session::{BACKGROUND, Entry, Item, Shell};
use crate::theme;
use crate::ui::{tool_icon, tool_summary};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::cell::Cell;

pub struct Lane {
    pub kind: String,
    pub title: String,
    pub state: State,
    pub steps: Vec<String>,
    pub report: String,
    /// A running shell's task id: `x` stops it.
    pub stop: Option<String>,
}

/// Every agent call in the transcript, in the order they were made, then the shell commands.
pub fn collect(items: &[Item], shells: &[Shell], cwd: &str) -> Vec<Lane> {
    let step = |name: &str, input: &Value| format!("{} {}", tool_icon(name).unwrap_or(name), tool_summary(name, input, cwd));
    let mut lanes = vec![];
    for (i, it) in items.iter().enumerate() {
        let Entry::Tool { name, input, result, nested: false, .. } = &it.entry else { continue };
        if !matches!(name.as_str(), "Agent" | "Task") {
            continue;
        }
        let working = result.as_ref().is_none_or(|r| r.0 == BACKGROUND);
        let state = match result {
            _ if working => State::Working,
            Some((_, true)) => State::Failed,
            _ => State::Done,
        };
        // A finished agent's steps are folded into its call; a running one's follow it, nested.
        let mut steps: Vec<String> = input["_steps"].as_array().into_iter().flatten().map(|s| step(s["name"].as_str().unwrap_or("?"), &s["input"])).collect();
        steps.extend(items[i + 1..].iter().map_while(|x| match &x.entry {
            Entry::Tool { name, input, nested: true, .. } => Some(step(name, input)),
            _ => None,
        }));
        lanes.push(Lane {
            kind: input["subagent_type"].as_str().unwrap_or("general-purpose").into(),
            title: tool_summary(name, input, cwd),
            state,
            steps,
            report: result.as_ref().filter(|_| !working).map(|r| r.0.clone()).unwrap_or_default(),
            stop: None,
        });
    }
    for s in shells {
        // Commands that end with their turn are not worth a row; backgrounded and running ones are.
        if !s.background && s.status.is_some() {
            continue;
        }
        let state = match s.status.as_deref() {
            None => State::Working,
            Some("completed") => State::Done,
            Some(_) => State::Failed,
        };
        let how = s.status.as_deref().map_or(String::new(), |st| format!("({st}) "));
        lanes.push(Lane {
            kind: if s.background { "shell, background" } else { "shell" }.into(),
            title: s.desc.clone(),
            state,
            steps: vec![],
            report: format!("{how}{}", s.summary),
            stop: s.status.is_none().then(|| s.task_id.clone()),
        });
    }
    lanes
}

/// Cursor in the list; once an agent is opened, how far its detail is scrolled.
#[derive(Default)]
pub struct Ui {
    cursor: usize,
    open: bool,
    scroll: usize,
    /// Detail rows at the width last drawn: the draw knows the wrapping, the key handler needs the bound.
    rows: Cell<usize>,
    /// `x` on a running shell: which row to stop, for the caller to act on.
    pub stop: Option<usize>,
}

impl Ui {
    /// `false` once the panel should close. `n` is how many agents there are.
    pub fn key(&mut self, k: KeyEvent, n: usize) -> bool {
        let ctrl_c = crate::panel::is_ctrl_c(&k);
        let back = k.code == KeyCode::Esc;
        if ctrl_c || (back && !self.open) {
            return false;
        }
        if self.open {
            self.key_in_detail(k);
        } else {
            self.key_in_list(k, n);
        }
        true
    }

    fn key_in_list(&mut self, k: KeyEvent, n: usize) {
        if n == 0 {
            return;
        }
        self.cursor = self.cursor.min(n - 1);
        match k.code {
            KeyCode::Up => self.cursor = cycle(self.cursor, n, true),
            KeyCode::Down => self.cursor = cycle(self.cursor, n, false),
            KeyCode::Enter => (self.open, self.scroll) = (true, 0),
            KeyCode::Char('x') => self.stop = Some(self.cursor),
            _ => {}
        }
    }

    fn key_in_detail(&mut self, k: KeyEvent) {
        let last = self.rows.get().saturating_sub(1);
        self.scroll = match k.code {
            KeyCode::Esc => {
                self.open = false;
                return;
            }
            KeyCode::Up => self.scroll.saturating_sub(1),
            KeyCode::Down => (self.scroll + 1).min(last),
            KeyCode::PageUp => self.scroll.saturating_sub(10),
            KeyCode::PageDown => (self.scroll + 10).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => self.scroll,
        };
    }
}

fn state_style(s: State) -> ratatui::style::Style {
    match s {
        State::Working => theme::working(),
        State::Done => theme::ok(),
        State::Failed => theme::danger(),
        _ => theme::dim(),
    }
}

pub fn view(ui: &Ui, lanes: &[Lane], width: usize) -> View {
    if lanes.is_empty() {
        let body = vec![Line::styled("no subagents or background commands in this tab yet", theme::dim())];
        return View::new("Tasks", body, 0, theme::hints(&[("esc", "close")]));
    }
    let cursor = ui.cursor.min(lanes.len() - 1);
    if ui.open { detail_view(ui, &lanes[cursor], width) } else { list_view(lanes, cursor) }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn list_view(lanes: &[Lane], cursor: usize) -> View {
    let body = lanes
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let steps = if l.steps.is_empty() { String::new() } else { format!("  {}", plural(l.steps.len(), "step")) };
            panel::row(i == cursor, vec![
                Span::styled(l.state.glyph(), state_style(l.state)),
                Span::raw(" "),
                Span::styled(format!("{:<16}", l.kind), theme::dim()),
                Span::raw(l.title.clone()),
                Span::styled(steps, theme::dim()),
            ])
        })
        .collect();
    let running = lanes.iter().filter(|l| l.state == State::Working).count();
    let shells = lanes.iter().filter(|l| l.kind.starts_with("shell")).count();
    let what = if shells == 0 { plural(lanes.len(), "agent") } else { format!("{}, {}", plural(lanes.len() - shells, "agent"), plural(shells, "command")) };
    let title = format!("Tasks · {what}, {running} running");
    let mut keys = vec![("↑↓", "move"), ("enter", "open")];
    if lanes[cursor].stop.is_some() {
        keys.push(("x", "stop"));
    }
    keys.push(("esc", "close"));
    let done = lanes.iter().filter(|l| !l.state.live()).count();
    let head = if lanes.len() > 1 { vec![Line::styled(format!("  {} done", crate::motion::count_bar(done, lanes.len(), 10)), theme::dim())] } else { vec![] };
    View::new(title, body, cursor, theme::hints(&keys)).with_head(head)
}

fn detail_view(ui: &Ui, l: &Lane, width: usize) -> View {
    let mut body: Vec<Line<'static>> = l.steps.iter().map(|s| Line::styled(format!("  {s}"), theme::dim())).collect();
    if l.report.is_empty() {
        body.push(Line::styled(if l.state == State::Working { "  working…" } else { "  (no report)" }, theme::dim()));
    } else {
        body.push(Line::raw(""));
        body.extend(crate::markdown::render(&l.report, width.saturating_sub(6)));
    }
    ui.rows.set(body.len());
    let head = vec![Line::from(vec![Span::styled(l.state.glyph(), state_style(l.state)), Span::raw(" "), Span::styled(l.title.clone(), theme::bold()), Span::styled(format!("  {} · {}", l.kind, l.state.label()), theme::dim())])];
    let focus = ui.scroll.min(body.len() - 1);
    View::new("Task", body, focus, theme::hints(&[("↑↓", "scroll"), ("esc", "back")])).with_head(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use serde_json::json;

    fn tool(id: &str, name: &str, input: Value, result: Option<(&str, bool)>, nested: bool) -> Item {
        Item { entry: Entry::Tool { id: id.into(), name: name.into(), input, result: result.map(|r| (r.0.into(), r.1)), nested }, cache: None, flipped: false }
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn running_backgrounded_done_and_failed_agents_each_get_a_state_with_their_steps_and_report() {
        let items = vec![
            tool("a", "Agent", json!({"description": "count lines", "subagent_type": "Explore"}), None, false),
            tool("s1", "Bash", json!({"command": "wc -l *.rs"}), None, true),
            tool("b", "Agent", json!({"description": "later"}), Some((BACKGROUND, false)), false),
            tool("c", "Agent", json!({"description": "wrote it", "_steps": [{"name": "Read", "input": {"file_path": "/p/a.rs"}}]}), Some(("all good", false)), false),
            tool("d", "Task", json!({"description": "broke"}), Some(("boom", true)), false),
            tool("x", "Bash", json!({"command": "ls"}), Some(("a", false)), false),
        ];
        let l = collect(&items, &[], "/p");
        assert_eq!(l.len(), 4, "only agent calls are lanes");
        assert_eq!((l[0].state, l[0].kind.as_str(), l[0].steps.len(), l[0].report.as_str()), (State::Working, "Explore", 1, ""), "its running step follows it");
        assert_eq!((l[1].state, l[1].report.as_str(), l[1].kind.as_str()), (State::Working, "", "general-purpose"), "the placeholder is not a report");
        assert_eq!((l[2].state, l[2].report.as_str(), l[2].steps.as_slice()), (State::Done, "all good", &["≡ a.rs".to_string()][..]));
        assert_eq!((l[3].state, l[3].report.as_str()), (State::Failed, "boom"));
    }

    #[test]
    fn keys_move_in_the_list_open_one_scroll_within_it_and_back_out_before_closing() {
        let mut ui = Ui::default();
        assert!(ui.key(key(KeyCode::Down), 3) && ui.cursor == 1);
        assert!(ui.key(key(KeyCode::Up), 3) && ui.key(key(KeyCode::Up), 3) && ui.cursor == 2, "wraps");
        assert!(ui.key(key(KeyCode::Enter), 3) && ui.open);
        ui.rows.set(5);
        for _ in 0..9 {
            ui.key(key(KeyCode::Down), 3);
        }
        assert_eq!(ui.scroll, 4, "stops at the last row");
        assert!(ui.key(key(KeyCode::Esc), 3) && !ui.open, "esc goes back to the list first");
        assert!(!ui.key(key(KeyCode::Esc), 3), "then closes");
        assert!(Ui::default().key(key(KeyCode::Enter), 0), "nothing to open in an empty list, and it stays up");
    }
}
