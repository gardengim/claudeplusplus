//! `/config` (also `/setting`): claude++'s own settings. Each setting is a short list of choices, ←→ steps
//! through them, the change applies at once and is saved to `~/.config/claude++/config.json` (a command-line
//! flag still wins for that run). Items are listed on tabs (Tab / Shift+Tab) by their `group`. Adding one is an
//! entry in [`ITEMS`] (with its group, one of [`GROUPS`]) plus an arm in `App::apply_setting`.
//! Claude Code's own settings are not edited here: `/config` in claude itself, or its settings files.

use crate::panel::{View, row_with, step_tab};
use crate::theme;
use crate::ui::wrap;
use ratatui::text::{Line, Span};

pub struct Item {
    /// Key in the config file.
    pub key: &'static str,
    /// The tab it is listed on: one of [`GROUPS`].
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    /// (stored value, shown label)
    pub choices: &'static [(&'static str, &'static str)],
}

pub const ITEMS: &[Item] = &[
    Item {
        key: "input_mode",
        group: "General",
        label: "Input mode",
        help: "default: the ordinary keys. vim: the message box has vim's modes; Esc leaves insert mode (hjkl w b e f t, d c y with motions and objects, p u, v selection, / searches). Enter still sends. /vim toggles too. editor: the box is your $VISUAL/$EDITOR (or `editor` in config.json) running inside claude++, always open in the message area: quit it to send; ctrl+\\ cancels; no / or @ completion; pgup/pgdn, alt+1..9 and, before you type, ↑↓ ← esc stay claude++'s. /editor toggles too.",
        choices: &[("default", "default"), ("vim", "vim"), ("editor", "editor")],
    },
    Item {
        key: "queue",
        group: "General",
        label: "While Claude works",
        help: "queue: a message you send waits in line and goes out when the turn ends (alt+q to reorder, edit or send now). send: it goes to Claude at once, which takes it in at its next step. Slash commands always go at once.",
        choices: &[("on", "queue"), ("off", "send")],
    },
    Item {
        key: "shell",
        group: "General",
        label: "Shell for !",
        help: "login: a command after ! runs in your own shell ($SHELL: fish, nushell, zsh, bash...), so it means what it means in your terminal. sh: plain POSIX sh. A shell claude++ does not know falls back to sh. Claude's own Bash tool uses the shell Claude Code picks (bash or zsh), and the status line command stays on bash/zsh.",
        choices: &[("login", "login shell"), ("sh", "sh")],
    },
    Item {
        key: "notify",
        group: "General",
        label: "Background tabs",
        help: "How a tab you are not looking at gets your attention when it finishes a turn or needs an answer. bell: the terminal bell. desktop: a desktop notification (terminals that support OSC 9, such as iTerm2, Ghostty and WezTerm). off: tab marker only.",
        choices: &[("off", "off"), ("bell", "bell"), ("desktop", "desktop")],
    },
    Item {
        key: "statusline",
        group: "General",
        label: "Status line",
        help: "builtin: claude++'s own line, nothing is spawned. claude: run the status line from your Claude Code settings (richer, one short-lived process per refresh). /statusline items chooses what the builtin line shows and in what order.",
        choices: &[("builtin", "builtin"), ("claude", "claude")],
    },
    Item {
        key: "theme",
        group: "Appearance",
        label: "Colors",
        help: "auto: match the terminal's background, light or dark, and follow it when it switches while claude++ runs (re-checked every few seconds while you are not typing). dark / light: fixed, for a terminal that does not report its background.",
        choices: &[("auto", "auto"), ("dark", "dark"), ("light", "light")],
    },
    Item {
        key: "spinner",
        group: "Appearance",
        label: "Spinner",
        help: "The mark beside what Claude is doing. Put your own in config.json as `spinner`: every character is a frame, or separate multi-character frames with spaces (`[=  ] [== ] [===]`); `none` hides it.",
        choices: &[(crate::motion::SPINNER, "stars"), ("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏", "dots"), ("|/-\\", "line"), ("none", "off")],
    },
    Item {
        key: "progress",
        group: "Appearance",
        label: "Progress bars",
        help: "The bars for usage, context and task progress. Put your own in config.json as `progress`: two characters, filled then empty (`#-`). off keeps the numbers and drops the bar.",
        choices: &[(crate::motion::BAR, "blocks"), ("▰▱", "pills"), ("#-", "ascii"), ("none", "off")],
    },
    Item {
        key: "motion",
        group: "Appearance",
        label: "Motion",
        help: "full: the spinner turns and a glint sweeps the word beside it. calm: the spinner only. still: nothing moves, and while Claude works the screen is redrawn once a second instead of about eight times.",
        choices: &[("full", "full"), ("calm", "calm"), ("off", "still")],
    },
    Item {
        key: "images",
        group: "Appearance",
        label: "Inline images",
        help: "auto: draw pictures in kitty and Ghostty, show a label elsewhere. on / off force it. A CLAUDEPP_IMAGES environment variable overrides this.",
        choices: &[("auto", "auto"), ("on", "on"), ("off", "off")],
    },
    Item {
        key: "mouse",
        group: "Appearance",
        label: "Mouse",
        help: "on: the wheel scrolls the conversation and dragging selects text (copied when you let go; double click a word, triple click a line). Hold Shift (Option in some terminals) to select with the terminal instead. off: the terminal keeps the mouse; scroll with pgup/pgdn.",
        choices: &[("on", "on"), ("off", "off")],
    },
    Item {
        key: "side_panel",
        group: "Appearance",
        label: "Side panel",
        help: "The full-screen session list is always there (← at the start of the input, or /sessions). on: ← first docks a list beside the conversation, a second ← goes full screen, → closes either; /sessions toggles the panel.",
        choices: &[("off", "off"), ("on", "on")],
    },
    Item {
        key: "profile",
        group: "Sessions",
        label: "Profile for new tabs",
        help: "full: stock Claude Code. sources: skip your user-level CLAUDE.md, skills and hooks. lean: also core tools only and no MCP (about 80% fewer prompt tokens). Applies to tabs opened from now on.",
        choices: &[("full", "full"), ("sources", "sources"), ("lean", "lean")],
    },
    Item {
        key: "share_mcp",
        group: "Sessions",
        label: "Share MCP servers",
        help: "off: every tab starts its own copy of each MCP server you defined (codegraph alone is four processes, 13 to 92 MB per server). on: tabs in the same folder share one copy, started by claude++ and stopped with the last tab. In this mode a tab loads only the servers in your own config files, so MCP servers from plugins and claude.ai connectors are left out; a shared server cannot send progress or list-changed notices; one that keeps state per conversation should not be shared. Applies to conversations started or resumed from now on.",
        choices: &[("off", "off"), ("on", "on")],
    },
    Item {
        key: "agent_term",
        group: "Sessions",
        label: "Agent terminal",
        help: "on: Claude gets terminals it can drive (term_open, term_send, term_screen, term_list, term_close), hosted inside claude++: a real pty per terminal, so it can use REPLs, vim, ssh, debuggers and servers it keeps watching, which its Bash tool cannot. You can watch and type in them with /term. Costs roughly 650 tokens of tool definitions per request while on, and every call goes through the usual permission prompt. Terminals you open yourself stay private until you share one in the /term list. Applies to conversations started or resumed from now on.",
        choices: &[("off", "off"), ("on", "on")],
    },
    Item {
        key: "cache_ttl_min",
        group: "Cost & memory",
        label: "Prompt cache lifetime",
        help: "Minutes the cache clock in the status line counts down from: 60 for the 1-hour cache, 5 for the short one.",
        choices: &[("5", "5 min"), ("60", "60 min")],
    },
    Item {
        key: "autocompact_k",
        group: "Cost & memory",
        label: "Compact the conversation at",
        help: "Every turn re-reads the whole conversation from the prompt cache, at a tenth of the price but still the largest cost of a long session. Claude Code compacts (summarizes) it by itself only near the model's full window, about 967k tokens. Choose a lower size to have it done earlier and bound that cost, at the price of working from a summary sooner. Applies to conversations started or resumed from now on.",
        choices: &[("0", "Claude Code's own (~967k)"), ("150", "150k"), ("200", "200k"), ("300", "300k"), ("400", "400k"), ("600", "600k")],
    },
    Item {
        key: "precompact_k",
        group: "Cost & memory",
        label: "Compact before the cache expires",
        help: "When a tab sits idle with a conversation at least this big and its prompt cache is about to expire, claude++ sends /compact while the cache is still warm. The summary is then read from the warm cache at a tenth of the price, and your next message re-caches only the summary instead of the whole conversation (1.25x to 2x the price per token). The cost is working from a summary, and a compaction wasted if you come back in the last seconds. Off by default; a tab you are typing in is left alone.",
        choices: &[("0", "off"), ("100", "100k"), ("200", "200k"), ("300", "300k"), ("400", "400k")],
    },
    Item {
        key: "idle_suspend_s",
        group: "Cost & memory",
        label: "Stop idle tabs after",
        help: "A background tab idle this long has its claude process stopped to free memory. It resumes on your next message.",
        choices: &[("0", "never"), ("120", "2 min"), ("300", "5 min"), ("600", "10 min"), ("1800", "30 min"), ("3600", "1 hour")],
    },
    Item {
        key: "mem_budget_mb",
        group: "Cost & memory",
        label: "Memory budget",
        help: "When all claude processes together use more than this, the least recently used idle tab is stopped.",
        choices: &[("0", "off"), ("500", "500 MB"), ("1000", "1 GB"), ("2000", "2 GB"), ("4000", "4 GB")],
    },
    Item {
        key: "limit_warn",
        group: "Rate limits",
        label: "Warn at",
        help: "Your plan has a 5-hour and a 7-day window (the status line's limits segment, /usage). When one first reaches this much, claude++ says so once: a note in the conversation, a message at the bottom and, if Background tabs is set, a bell or desktop notice. It warns again only after that window resets.",
        choices: &[("0", "off"), ("70", "70%"), ("80", "80%"), ("90", "90%"), ("95", "95%")],
    },
    Item {
        key: "limit_hold",
        group: "Rate limits",
        label: "Hold messages at",
        help: "When a window is at least this full, a message you send is not sent: it waits in the queue, due when that window resets (alt+q shows it; Send now there goes ahead anyway, for the rest of that window). Slash commands still go at once, and a turn already running is not touched. Needs Claude Code to have reported the window: the limits segment shows it.",
        choices: &[("0", "off"), ("90", "90%"), ("95", "95%"), ("99", "99%"), ("100", "100%")],
    },
];

/// The tabs, in order. Every item's `group` is one of these.
pub const GROUPS: &[&str] = &["General", "Appearance", "Sessions", "Cost & memory", "Rate limits"];

#[derive(Default)]
pub struct Ui {
    /// ←→ drives the section strip instead of the value of the row (Tab moves between the two).
    pub strip: bool,
    pub tab: usize,
    /// Row within the tab.
    pub sel: usize,
}

impl Ui {
    /// Indices into [`ITEMS`] of the current tab's items, in order.
    pub fn items(&self) -> Vec<usize> {
        let group = GROUPS[self.tab.min(GROUPS.len() - 1)];
        (0..ITEMS.len()).filter(|&i| ITEMS[i].group == group).collect()
    }
    /// The item under the cursor, as an index into [`ITEMS`].
    pub fn current(&self) -> usize {
        let items = self.items();
        items[self.sel.min(items.len() - 1)]
    }
    pub fn step(&mut self, delta: isize) {
        self.sel = step_tab(self.sel, self.items().len(), delta);
    }
    pub fn switch(&mut self, delta: isize) {
        self.tab = step_tab(self.tab, GROUPS.len(), delta);
        self.sel = 0;
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
    let items = ui.items();
    let sel = ui.sel.min(items.len() - 1);
    let label_w = ITEMS.iter().map(|i| i.label.len()).max().unwrap_or(0) + 2;
    let body: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(n, &i)| {
            let item = &ITEMS[i];
            let on = n == sel;
            let value = current(item.key);
            let mut spans = vec![Span::styled(format!("{:<label_w$}", item.label), if on { theme::bold() } else { Default::default() })];
            for (v, label) in item.choices {
                let active = *v == value;
                spans.push(theme::chip_in(label, active, active && on, !ui.strip, theme::bold()));
            }
            if !item.choices.iter().any(|c| c.0 == value) {
                spans.push(theme::chip_in(&value, true, on, !ui.strip, theme::bold())); // e.g. set by a flag
            }
            if ui.strip { crate::panel::row_in(on, false, spans) } else { row_with(on, false, spans) }
        })
        .collect();
    let head = vec![crate::panel::tab_strip_focus(GROUPS.iter().map(|g| (g.to_string(), theme::dim())).collect(), ui.tab, ui.strip), Line::raw("")];
    let help_of = |i: usize| format!("{}  Saved in ~/.config/claude++/config.json as {}.", ITEMS[i].help, ITEMS[i].key);
    const HELP_MAX: usize = 6;
    // the longest help of any tab, so the panel keeps one height when you switch tabs too
    let help_rows = (0..ITEMS.len()).map(|i| wrap(&help_of(i), inner).len()).max().unwrap_or(1).min(HELP_MAX);
    let mut help = wrap(&theme::keys(&help_of(items[sel])), inner);
    if help.len() > help_rows {
        help.truncate(help_rows);
        if let Some(l) = help.last_mut() {
            l.push('…');
        }
    }
    help.resize(help_rows, String::new());
    let mut tail = vec![Line::raw("")];
    tail.extend(help.into_iter().map(|l| Line::styled(l, theme::dim())));
    let footer = theme::hints(&[("↑↓←→", "move"), ("tab", "next menu"), ("enter", "change"), ("esc", "close")]);
    let rows = GROUPS.iter().map(|g| ITEMS.iter().filter(|i| i.group == *g).count()).max().unwrap_or(0);
    View::new("Config", body, sel, footer).with_head(head).with_tail(tail).rows(rows, rows.max(16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycling_wraps_and_recovers_from_unknown_values() {
        let item = ITEMS.iter().find(|i| i.key == "profile").unwrap(); // full, sources, lean
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
    fn every_item_is_on_exactly_one_tab_and_no_tab_is_empty() {
        assert!(ITEMS.iter().all(|i| GROUPS.contains(&i.group)), "an item names a tab that does not exist");
        let mut ui = Ui::default();
        let mut seen: Vec<usize> = vec![];
        for _ in GROUPS {
            assert!(!ui.items().is_empty());
            seen.extend(ui.items());
            ui.switch(1);
        }
        seen.sort();
        assert_eq!(seen, (0..ITEMS.len()).collect::<Vec<_>>());
        assert_eq!(ui.tab, 0, "tabs wrap");
        ui.switch(-1);
        assert_eq!(ui.tab, GROUPS.len() - 1);
    }

    #[test]
    fn the_help_keeps_one_height_at_every_width() {
        let mut ui = Ui::default();
        for w in [60, 100, 200] {
            let mut heights = vec![];
            for t in 0..GROUPS.len() {
                ui.tab = t;
                for n in 0..ui.items().len() {
                    ui.sel = n;
                    let v = view(&ui, w, |_| "x".into());
                    heights.push((v.tail.len(), v.head.len()));
                }
            }
            assert!(heights.windows(2).all(|p| p[0] == p[1]), "width {w}");
        }
        ui.tab = 0;
        ui.sel = 0;
        ui.step(-1);
        assert_eq!(ui.sel, ui.items().len() - 1, "wraps within the tab");
    }
}
