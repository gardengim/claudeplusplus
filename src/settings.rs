//! claude++'s own config, shown in a panel (`/config`). Each setting is a short list of choices; ←→ steps
//! through them, the change applies at once and is saved to `~/.config/claude++/config.json`. A command-line
//! flag still wins for that run. Adding a setting is one entry in [`ITEMS`] plus one arm in
//! `App::apply_setting`.

use crate::panel::{View, row_with};
use crate::theme;
use crate::ui::wrap;
use ratatui::text::{Line, Span};

pub struct Item {
    /// Key in the config file.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    /// (stored value, shown label)
    pub choices: &'static [(&'static str, &'static str)],
}

pub const ITEMS: &[Item] = &[
    Item {
        key: "statusline",
        label: "Status line",
        help: "builtin: claude++'s own line, nothing is spawned. claude: run the status line from your Claude Code settings (richer, one short-lived process per refresh).",
        choices: &[("builtin", "builtin"), ("claude", "claude")],
    },
    Item {
        key: "profile",
        label: "Profile for new tabs",
        help: "full: stock Claude Code. sources: skip your user-level CLAUDE.md, skills and hooks. lean: also core tools only and no MCP (about 80% fewer prompt tokens). Applies to tabs opened from now on.",
        choices: &[("full", "full"), ("sources", "sources"), ("lean", "lean")],
    },
    Item {
        key: "images",
        label: "Inline images",
        help: "auto: draw pictures in kitty and Ghostty, show a label elsewhere. on / off force it. A CLAUDEPP_IMAGES environment variable overrides this.",
        choices: &[("auto", "auto"), ("on", "on"), ("off", "off")],
    },
    Item {
        key: "cache_ttl_min",
        label: "Prompt cache lifetime",
        help: "Minutes the cache clock in the status line counts down from: 60 for the 1-hour cache, 5 for the short one.",
        choices: &[("5", "5 min"), ("60", "60 min")],
    },
    Item {
        key: "idle_suspend_s",
        label: "Stop idle tabs after",
        help: "A background tab idle this long has its claude process stopped to free memory. It resumes on your next message.",
        choices: &[("0", "never"), ("300", "5 min"), ("600", "10 min"), ("1800", "30 min"), ("3600", "1 hour")],
    },
    Item {
        key: "mem_budget_mb",
        label: "Memory budget",
        help: "When all claude processes together use more than this, the least recently used idle tab is stopped.",
        choices: &[("0", "off"), ("500", "500 MB"), ("1000", "1 GB"), ("2000", "2 GB"), ("4000", "4 GB")],
    },
    Item {
        key: "mouse",
        label: "Mouse wheel",
        help: "on: the wheel scrolls the conversation; selecting text then needs Shift held (Option in some terminals). off: the terminal keeps the mouse, scroll with pgup/pgdn.",
        choices: &[("on", "scroll"), ("off", "off")],
    },
    Item {
        key: "side_panel",
        label: "Side panel",
        help: "The full-screen session list is always there (← at the start of the input, or ctrl+r). on: ← first docks a list beside the conversation, a second ← goes full screen, → closes either; ctrl+r or /sessions toggles the panel.",
        choices: &[("off", "off"), ("on", "on")],
    },
];

/// Where the cursor is.
#[derive(Clone, Copy, Default)]
pub struct Ui {
    pub sel: usize,
}

impl Ui {
    pub fn step(&mut self, delta: isize) {
        self.sel = (self.sel as isize + delta).rem_euclid(ITEMS.len() as isize) as usize;
    }
}

/// The value `delta` steps from `current` (wrapping). A current value that is not one of the choices (set
/// by a flag, say) steps onto the first or last choice.
pub fn cycle(item: &Item, current: &str, delta: isize) -> &'static str {
    let n = item.choices.len() as isize;
    let at = item.choices.iter().position(|c| c.0 == current).map_or(if delta > 0 { -1 } else { n }, |i| i as isize);
    item.choices[(at + delta).rem_euclid(n) as usize].0
}

/// `width` is the terminal width. The help text wraps and always takes as many lines as the longest one
/// needs, so the panel does not change height as the cursor moves.
pub fn view(ui: Ui, width: usize, current: impl Fn(&str) -> String) -> View {
    let inner = width.saturating_sub(4).max(20);
    let help_rows = ITEMS.iter().map(|i| wrap(i.help, inner).len()).max().unwrap_or(1);
    let width = ITEMS.iter().map(|i| i.label.len()).max().unwrap_or(0) + 2;
    let body: Vec<Line> = ITEMS
        .iter()
        .enumerate()
        .map(|(n, item)| {
            let on = n == ui.sel;
            let value = current(item.key);
            let mut spans = vec![Span::styled(format!("{:<width$}", item.label), if on { theme::bold() } else { Default::default() })];
            let known = item.choices.iter().any(|c| c.0 == value);
            for (v, label) in item.choices {
                let active = *v == value;
                spans.push(Span::styled(format!(" {label} "), if active && on { theme::selected() } else if active { theme::bold() } else { theme::dim() }));
            }
            if !known {
                spans.push(Span::styled(format!(" {value} "), if on { theme::selected() } else { theme::bold() })); // e.g. set by a flag
            }
            row_with(on, false, spans)
        })
        .collect();
    let mut tail = vec![Line::raw("")];
    let mut help = wrap(ITEMS[ui.sel.min(ITEMS.len() - 1)].help, inner);
    help.resize(help_rows, String::new());
    tail.extend(help.into_iter().map(|l| Line::styled(l, theme::dim())));
    let footer = theme::hints(&[("↑↓", "move"), ("←→", "change"), ("esc", "close")]);
    View::new("Config", body, ui.sel, footer).with_tail(tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycling_wraps_and_recovers_from_unknown_values() {
        let item = &ITEMS[1]; // full, sources, lean
        assert_eq!(cycle(item, "full", 1), "sources");
        assert_eq!(cycle(item, "lean", 1), "full");
        assert_eq!(cycle(item, "full", -1), "lean");
        assert_eq!(cycle(item, "weird", 1), "full", "an unknown value steps onto the first choice");
        assert_eq!(cycle(item, "weird", -1), "lean");
    }

    #[test]
    fn keys_are_unique_and_choices_nonempty() {
        let mut keys: Vec<_> = ITEMS.iter().map(|i| i.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), ITEMS.len());
        assert!(ITEMS.iter().all(|i| !i.choices.is_empty() && !i.help.is_empty()));
    }

    #[test]
    fn the_view_marks_the_active_choice_and_shows_help_for_the_row() {
        let v = view(Ui { sel: 1 }, 100, |k| if k == "profile" { "lean".into() } else { "x".into() });
        let text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(text(&v.body[1]).starts_with("❯ Profile for new tabs"));
        assert!(text(&v.body[0]).starts_with("  Status line"));
        let lean = v.body[1].spans.iter().find(|s| s.content.contains("lean")).unwrap();
        assert_eq!(lean.style, theme::selected(), "the active choice of the cursor row is highlighted");
        let full = v.body[1].spans.iter().find(|s| s.content.contains("full")).unwrap();
        assert_eq!(full.style, theme::dim());
        assert!(text(&v.body[0]).contains(" x "), "a value outside the choices is still shown");
        assert!(text(&v.tail[1]).contains("full: stock Claude Code"));
        // the panel keeps one height whichever row is highlighted, at any width
        for w in [60, 100, 160] {
            let heights: Vec<usize> = (0..ITEMS.len()).map(|n| view(Ui { sel: n }, w, |_| "x".into()).tail.len()).collect();
            assert!(heights.windows(2).all(|p| p[0] == p[1]), "width {w}: {heights:?}");
        }
        let all = (0..ITEMS.len()).flat_map(|n| view(Ui { sel: n }, 60, |_| "x".into()).tail.iter().map(text).collect::<Vec<_>>()).collect::<Vec<_>>().join(" ");
        assert!(all.contains("(richer, one short-lived process per refresh)") || all.contains("per refresh"), "help text is wrapped, not cut: {all}");
    }
}
