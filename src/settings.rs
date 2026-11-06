//! `/config` (also `/setting`): claude++'s own settings. Each setting is a short list of choices, ←→ steps
//! through them, the change applies at once and is saved to `~/.config/claude++/config.json` (a command-line
//! flag still wins for that run). Adding one is an entry in [`ITEMS`] plus an arm in `App::apply_setting`.
//! Claude Code's own settings are not edited here: `/config` in claude itself, or its settings files.

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
        help: "builtin: claude++'s own line, nothing is spawned. claude: run the status line from your Claude Code settings (richer, one short-lived process per refresh). /statusline items chooses what the builtin line shows and in what order.",
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
        choices: &[("0", "never"), ("120", "2 min"), ("300", "5 min"), ("600", "10 min"), ("1800", "30 min"), ("3600", "1 hour")],
    },
    Item {
        key: "mem_budget_mb",
        label: "Memory budget",
        help: "When all claude processes together use more than this, the least recently used idle tab is stopped.",
        choices: &[("0", "off"), ("500", "500 MB"), ("1000", "1 GB"), ("2000", "2 GB"), ("4000", "4 GB")],
    },
    Item {
        key: "autocompact_k",
        label: "Compact the conversation at",
        help: "Every turn re-reads the whole conversation from the prompt cache, at a tenth of the price but still the largest cost of a long session. Claude Code compacts (summarizes) it by itself only near the model's full window, about 967k tokens. Choose a lower size to have it done earlier and bound that cost, at the price of working from a summary sooner. Applies to conversations started or resumed from now on.",
        choices: &[("0", "Claude Code's own (~967k)"), ("150", "150k"), ("200", "200k"), ("300", "300k"), ("400", "400k"), ("600", "600k")],
    },
    Item {
        key: "precompact_k",
        label: "Compact before the cache expires",
        help: "When a tab sits idle with a conversation at least this big and its prompt cache is about to expire, claude++ sends /compact while the cache is still warm. The summary is then read from the warm cache at a tenth of the price, and your next message re-caches only the summary instead of the whole conversation (1.25x to 2x the price per token). The cost is working from a summary, and a compaction wasted if you come back in the last seconds. Off by default; a tab you are typing in is left alone.",
        choices: &[("0", "off"), ("100", "100k"), ("200", "200k"), ("300", "300k"), ("400", "400k")],
    },
    Item {
        key: "share_mcp",
        label: "Share MCP servers",
        help: "off: every tab starts its own copy of each MCP server you defined (codegraph alone is four processes, 13 to 92 MB per server). on: tabs in the same folder share one copy, started by claude++ and stopped with the last tab. In this mode a tab loads only the servers in your own config files, so MCP servers from plugins and claude.ai connectors are left out; a shared server cannot send progress or list-changed notices; one that keeps state per conversation should not be shared. Applies to conversations started or resumed from now on.",
        choices: &[("off", "off"), ("on", "on")],
    },
    Item {
        key: "notify",
        label: "Background tabs",
        help: "How a tab you are not looking at gets your attention when it finishes a turn or needs an answer. bell: the terminal bell. desktop: a desktop notification (terminals that support OSC 9, such as iTerm2, Ghostty and WezTerm). off: tab marker only.",
        choices: &[("off", "off"), ("bell", "bell"), ("desktop", "desktop")],
    },
    Item {
        key: "theme",
        label: "Colors",
        help: "auto: match the terminal's background, light or dark, and follow it when it switches while claude++ runs (re-checked every few seconds while you are not typing). dark / light: fixed, for a terminal that does not report its background.",
        choices: &[("auto", "auto"), ("dark", "dark"), ("light", "light")],
    },
    Item {
        key: "mouse",
        label: "Mouse wheel",
        help: "on: clicks pick tabs and list rows (click the highlighted row to open it) and the wheel scrolls the conversation; selecting text then needs Shift held (Option in some terminals). off: the terminal keeps the mouse, scroll with pgup/pgdn.",
        choices: &[("on", "scroll"), ("off", "off")],
    },
    Item {
        key: "autocopy",
        label: "Copy on select",
        help: "Drag the mouse over the conversation to select text (needs Mouse wheel on). on: the selection is copied to the clipboard as you let go. off: ctrl+y copies it.",
        choices: &[("off", "off"), ("on", "on")],
    },
    Item {
        key: "side_panel",
        label: "Side panel",
        help: "The full-screen session list is always there (← at the start of the input, or /sessions). on: ← first docks a list beside the conversation, a second ← goes full screen, → closes either; /sessions toggles the panel.",
        choices: &[("off", "off"), ("on", "on")],
    },
    Item {
        key: "input_mode",
        label: "Input mode",
        help: "default: the ordinary keys. vim: the message box has vim's modes; Esc leaves insert mode (hjkl w b e f t, d c y with motions and objects, p u, v selection, / searches). Enter still sends. /vim toggles too. editor: the box is your $VISUAL/$EDITOR (or `editor` in config.json) running inside claude++, always open in the message area: quit it to send; ctrl+\\ cancels; no / or @ completion; pgup/pgdn, alt+1..9 and, before you type, ↑↓ ← esc stay claude++'s. /editor toggles too.",
        choices: &[("default", "default"), ("vim", "vim"), ("editor", "editor")],
    },
    Item {
        key: "queue",
        label: "While Claude works",
        help: "queue: a message you send waits in line and goes out when the turn ends (alt+q to reorder, edit or send now). send: it goes to Claude at once, which takes it in at its next step. Slash commands always go at once.",
        choices: &[("on", "queue"), ("off", "send")],
    },
];

#[derive(Default)]
pub struct Ui {
    pub sel: usize,
}

impl Ui {
    pub fn step(&mut self, delta: isize) {
        self.sel = (self.sel as isize + delta).rem_euclid(ITEMS.len() as isize) as usize;
    }
}

/// The value cycle for claude++'s own setting: the value `delta` steps from `current` (wrapping). A current
/// value that is not one of the choices (set by a flag, say) steps onto the first or last choice.
pub fn cycle(item: &Item, current: &str, delta: isize) -> &'static str {
    let n = item.choices.len() as isize;
    let at = item.choices.iter().position(|c| c.0 == current).map_or(if delta > 0 { -1 } else { n }, |i| i as isize);
    item.choices[(at + delta).rem_euclid(n) as usize].0
}

/// `width` is the terminal width. The help pane keeps one height while the cursor moves.
pub fn view(ui: &Ui, width: usize, current: impl Fn(&str) -> String) -> View {
    let inner = width.saturating_sub(4).max(20);
    let sel = ui.sel.min(ITEMS.len() - 1);
    let label_w = ITEMS.iter().map(|i| i.label.len()).max().unwrap_or(0) + 2;
    let body: Vec<Line> = ITEMS
        .iter()
        .enumerate()
        .map(|(n, item)| {
            let on = n == sel;
            let value = current(item.key);
            let mut spans = vec![Span::styled(format!("{:<label_w$}", item.label), if on { theme::bold() } else { Default::default() })];
            for (v, label) in item.choices {
                let active = *v == value;
                spans.push(theme::chip(label, active, active && on, theme::bold()));
            }
            if !item.choices.iter().any(|c| c.0 == value) {
                spans.push(theme::chip(&value, true, on, theme::bold())); // e.g. set by a flag
            }
            row_with(on, false, spans)
        })
        .collect();
    let help_of = |i: usize| format!("{}  Saved in ~/.config/claude++/config.json as {}.", ITEMS[i].help, ITEMS[i].key);
    const HELP_MAX: usize = 6;
    let help_rows = (0..ITEMS.len()).map(|i| wrap(&help_of(i), inner).len()).max().unwrap_or(1).min(HELP_MAX);
    let mut help = wrap(&theme::keys(&help_of(sel)), inner);
    if help.len() > help_rows {
        help.truncate(help_rows);
        if let Some(l) = help.last_mut() {
            l.push('…');
        }
    }
    help.resize(help_rows, String::new());
    let mut tail = vec![Line::raw("")];
    tail.extend(help.into_iter().map(|l| Line::styled(l, theme::dim())));
    let footer = theme::hints(&[("↑↓", "move"), ("←→", "change"), ("esc", "close")]);
    View::new("Config", body, sel, footer).with_tail(tail).rows(12, 16)
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
    fn the_help_keeps_one_height_at_every_width() {
        let mut ui = Ui::default();
        for w in [60, 100, 200] {
            let heights: Vec<usize> = (0..ITEMS.len()).map(|n| { ui.sel = n; view(&ui, w, |_| "x".into()).tail.len() }).collect();
            assert!(heights.windows(2).all(|p| p[0] == p[1]), "width {w}");
        }
        ui.sel = 0;
        ui.step(-1);
        assert_eq!(ui.sel, ITEMS.len() - 1, "wraps");
    }
}
