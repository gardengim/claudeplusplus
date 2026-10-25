//! Session (tab) config, shown in a panel (`/session`): the permission mode and, per tool, whether this tab
//! always allows it, always denies it, or leaves it to Claude Code (your settings and the mode). Rules are
//! sent as `permissions.allow/deny` through `apply_flag_settings`, which takes effect on the running
//! session, is replaced as a whole each time (so removing a rule clears it), and is re-applied if the tab's
//! process is restarted. They add to your `settings.json` rules and apply to this tab only.

use crate::panel::{View, row_with};
use crate::session::Session;
use crate::theme;
use crate::ui::wrap;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

pub const MODES: [&str; 4] = ["default", "acceptEdits", "plan", "auto"];
/// Offered before the session has reported its own tool list (that arrives with its first turn).
const BASE_TOOLS: [&str; 10] = ["Bash", "Read", "Edit", "Write", "Glob", "Grep", "WebFetch", "WebSearch", "NotebookEdit", "Agent"];

#[derive(Clone, Default)]
pub struct Ui {
    pub sel: usize,
    /// The rule being typed on the "add" row.
    pub adding: Option<String>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Row {
    Mode,
    /// Tool or rule name, and whether it is a rule of the user's own rather than a known tool.
    Tool(String, bool),
    Add,
}

pub fn rows(s: &Session) -> Vec<Row> {
    let mut names: Vec<String> = BASE_TOOLS.iter().map(|t| t.to_string()).collect();
    for t in &s.tools {
        if !names.contains(t) {
            names.push(t.clone());
        }
    }
    let custom = s.rules.iter().filter(|(r, _)| !names.contains(r)).map(|(r, _)| Row::Tool(r.clone(), true)).collect::<Vec<_>>();
    let mut out = vec![Row::Mode];
    out.extend(names.into_iter().map(|n| Row::Tool(n, false)));
    out.extend(custom);
    out.push(Row::Add);
    out
}

/// The next rule state `delta` steps on: default → allow → deny → default.
pub fn step(current: Option<bool>, delta: isize) -> Option<bool> {
    let order = [None, Some(true), Some(false)];
    let at = order.iter().position(|o| *o == current).unwrap_or(0) as isize;
    order[(at + delta).rem_euclid(3) as usize]
}

pub fn view(ui: &Ui, s: &Session, width: usize) -> View {
    let rows = rows(s);
    let sel = ui.sel.min(rows.len() - 1);
    let label_w = rows.iter().map(|r| match r { Row::Tool(n, _) => n.chars().count(), _ => 15 }).max().unwrap_or(0).min(40) + 2;
    let body: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(n, row)| {
            let on = n == sel;
            let name = |t: &str| Span::styled(format!("{t:<label_w$}"), if on { theme::bold() } else { Style::new() });
            let chip = |label: &str, active: bool, hue: Style| theme::chip(label, active, active && on, hue);
            match row {
                Row::Mode => {
                    let mut v = vec![name("Permission mode")];
                    v.extend(MODES.iter().map(|m| chip(m, *m == s.mode, theme::bold())));
                    row_with(on, false, v)
                }
                Row::Tool(t, _) => {
                    let state = s.rule(t);
                    row_with(on, false, vec![name(t), chip("default", state.is_none(), theme::bold()), chip("allow ✓", state == Some(true), theme::ok()), chip("deny ✗", state == Some(false), theme::danger())])
                }
                Row::Add => match (&ui.adding, on) {
                    (Some(text), true) => row_with(on, false, vec![Span::styled("rule ", theme::dim()), Span::raw(text.clone()), Span::styled(" ", theme::selected())]),
                    _ => row_with(on, false, vec![Span::styled("+ add a rule…", if on { theme::bold() } else { theme::dim() })]),
                },
            }
        })
        .collect();
    let help = match &rows[sel] {
        Row::Mode => "default asks before edits and commands; acceptEdits allows file edits; plan only plans; auto lets Claude Code's classifier decide. Same as shift+tab.",
        Row::Tool(_, true) => "A rule of your own, e.g. Bash(git push:*). ←→ steps it; stepping past deny removes it (d does too).",
        Row::Tool(..) => "allow: never ask for this tool. deny: refuse it. default: whatever your settings and the mode say. Applies to this tab only, now, and adds to your settings.json rules.",
        Row::Add => "Enter, then type a rule such as Bash(git push:*), mcp__server__tool or Read(./secrets/**); Enter adds it as allow, ←→ makes it deny.",
    };
    let mut tail = vec![Line::raw("")];
    tail.extend(wrap(help, width.saturating_sub(4).max(20)).into_iter().map(|l| Line::styled(l, theme::dim())));
    let footer = if ui.adding.is_some() { theme::hints(&[("enter", "add"), ("esc", "cancel")]) } else { theme::hints(&[("↑↓", "move"), ("←→", "change"), ("d", "remove"), ("esc", "close")]) };
    View::new("Session", body, sel, footer).with_tail(tail).rows(0, 14)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_cycle_default_allow_deny_and_back() {
        assert_eq!(step(None, 1), Some(true));
        assert_eq!(step(Some(true), 1), Some(false));
        assert_eq!(step(Some(false), 1), None);
        assert_eq!(step(None, -1), Some(false));
    }
}
