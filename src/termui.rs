//! `/term`: the terminals of this tab (see `term.rs`) on screen. `/term` opens a shell, or lists them when there are some;
//! `/term <command>` runs that command in one. A terminal you look at takes the whole screen and gets every key, except
//! ctrl+\ (back to the conversation; the program keeps running). The list is where you share one with the agent.

use crate::App;
use crate::panel::{self, View, row_with};
use crate::term::Term;
use crate::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use std::sync::Arc;
use std::sync::atomic::Ordering;

#[derive(Default)]
pub struct Ui {
    pub sel: usize,
    /// The row `x` was pressed on once: a second press closes it (ending whatever runs in it).
    pub armed: Option<usize>,
}

pub fn view(ui: &Ui, list: &[Arc<Term>], agent_on: bool) -> View {
    let sel = ui.sel.min(list.len().saturating_sub(1));
    let mut body: Vec<Line> = list
        .iter()
        .enumerate()
        .map(|(n, t)| {
            let state = t.exit().unwrap_or_else(|| "running".into());
            let who = if t.by_agent { "opened by Claude" } else if t.shared.load(Ordering::Relaxed) { "yours, shared with Claude" } else { "yours, private" };
            let mut tags = format!("{who} · {state}");
            if ui.armed == Some(n) {
                tags.push_str(" · x again closes it");
            }
            row_with(
                n == sel,
                false,
                vec![
                    Span::styled(format!("{:<10}", t.name), if n == sel { theme::bold() } else { Default::default() }),
                    Span::styled(format!("{:<24}", crate::ui::truncate(&t.command, 22)), theme::code()),
                    Span::styled(tags, if ui.armed == Some(n) { theme::danger() } else { theme::dim() }),
                ],
            )
        })
        .collect();
    if list.is_empty() {
        body.push(Line::from(Span::styled("no terminals: n opens a shell", theme::dim())));
    }
    if !agent_on {
        body.push(Line::from(Span::styled("Claude cannot use terminals: /config → Agent terminal", theme::dim())));
    }
    let footer = theme::hints(&[("↑↓", "move"), ("enter", "open"), ("n", "new shell"), ("s", "share with Claude"), ("x", "close"), ("esc", "back")]);
    View::new("Terminals", body, sel, footer).rows(4, 12)
}

impl App {
    /// `/term [command]`.
    pub(crate) fn term_cmd(&mut self, arg: &str) {
        let terms = self.sessions[self.active].terms.clone();
        terms.set_cwd(self.cwd());
        if arg.is_empty() && !terms.all().is_empty() {
            return self.term_ui = Some(Ui::default());
        }
        match terms.open(None, Some(arg).filter(|a| !a.is_empty()), false, None) {
            Ok(t) => self.term_view = Some(t),
            Err(e) => self.flash(e),
        }
    }

    pub(crate) fn term_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.term_ui.take() else { return };
        let terms = self.sessions[self.active].terms.clone();
        let list = terms.all();
        let n = list.len().max(1);
        let sel = ui.sel.min(n - 1);
        match k.code {
            KeyCode::Esc => return,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return,
            KeyCode::Up | KeyCode::Down => {
                ui.sel = panel::cycle(sel, n, k.code == KeyCode::Up);
                ui.armed = None;
            }
            KeyCode::Enter => {
                if let Some(t) = list.get(sel) {
                    self.term_view = Some(t.clone());
                    return;
                }
            }
            KeyCode::Char('n') => {
                terms.set_cwd(self.cwd());
                match terms.open(None, None, false, None) {
                    Ok(t) => {
                        self.term_view = Some(t);
                        return;
                    }
                    Err(e) => self.flash(e),
                }
            }
            KeyCode::Char('s') => {
                if let Some(t) = list.get(sel) {
                    let now = !t.shared.load(Ordering::Relaxed);
                    if t.by_agent && !now {
                        self.flash("Claude opened that one; it stays shared");
                    } else {
                        t.shared.store(now, Ordering::Relaxed);
                        self.flash(if now { format!("{} is shared: Claude can see and type in it", t.name) } else { format!("{} is private again", t.name) });
                    }
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => match list.get(sel) {
                Some(t) if ui.armed == Some(sel) => {
                    terms.close(&t.name);
                    ui.armed = None;
                }
                Some(_) => ui.armed = Some(sel),
                None => {}
            },
            _ => {}
        }
        self.term_ui = Some(ui);
    }

    /// A key while a terminal has the screen: all of them go to it, except ctrl+\ (a terminal that cannot tell it apart sends ctrl+4).
    pub(crate) fn term_view_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('\\' | '4')) {
            self.term_view = None;
            return;
        }
        if let Some(t) = &self.term_view {
            t.key(k);
        }
    }

    /// Output wakes the UI only for terminals someone looks at: the list or the viewer of the tab in front.
    pub(crate) fn sync_term_watch(&self) {
        for (i, s) in self.sessions.iter().enumerate() {
            s.terms.watch.store(i == self.active && (self.term_ui.is_some() || self.term_view.is_some()), Ordering::Relaxed);
        }
    }
}
