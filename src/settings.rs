//! `/config` (also `/setting`): one panel for claude++'s own settings and Claude Code's.
//!
//! The first tab is claude++'s: each setting is a short list of choices, ←→ steps through them, the change
//! applies at once and is saved to `~/.config/claude++/config.json` (a command-line flag still wins for that
//! run). Adding one is an entry in [`ITEMS`] plus an arm in `App::apply_setting`.
//!
//! The other tabs are every key a Claude Code settings file can hold, by topic, generated into `cc_keys.rs`
//! from the settings reference (`tools/gen_cc_settings.py`). They are written to the user, project or local
//! settings file (ctrl+s picks which); running sessions pick most of them up by themselves. Typing searches
//! every tab at once.

use crate::cc_keys::{KEYS, TABS};
use crate::panel::{View, row_with};
use crate::permissions::{Scope, read_json, write_json};
use crate::theme;
use crate::ui::wrap;
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};

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
        choices: &[("0", "never"), ("120", "2 min"), ("300", "5 min"), ("600", "10 min"), ("1800", "30 min"), ("3600", "1 hour")],
    },
    Item {
        key: "mem_budget_mb",
        label: "Memory budget",
        help: "When all claude processes together use more than this, the least recently used idle tab is stopped.",
        choices: &[("0", "off"), ("500", "500 MB"), ("1000", "1 GB"), ("2000", "2 GB"), ("4000", "4 GB")],
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
        help: "default: the ordinary keys. vim: the message box has vim's modes; Esc leaves insert mode (hjkl w b e f t, d c y with motions and objects, p u, v selection, / searches). Enter still sends. /vim toggles too. editor: the box is your $VISUAL/$EDITOR (or `editor` in config.json) running inside claude++, always open in the message area: quit it to send; ctrl+\\ cancels; no / or @ completion. /editor toggles too.",
        choices: &[("default", "default"), ("vim", "vim"), ("editor", "editor")],
    },
    Item {
        key: "queue",
        label: "While Claude works",
        help: "queue: a message you send waits in line and goes out when the turn ends (alt+q to reorder, edit or send now). send: it goes to Claude at once, which takes it in at its next step. Slash commands always go at once.",
        choices: &[("on", "queue"), ("off", "send")],
    },
];

/// How a Claude Code key's value is edited.
pub enum Kind {
    Bool,
    Choice(&'static [&'static str]),
    Number,
    Text,
    /// A list or an object, typed as JSON.
    Json,
}

/// A Claude Code settings key.
pub struct CcKey {
    /// Dotted for a field inside an object (`sandbox.enabled`).
    pub key: &'static str,
    /// Index into [`tabs`].
    pub tab: usize,
    pub kind: Kind,
    /// Which files it is read from, as the reference says (`Any file`, `User or managed`, ...).
    pub scope: &'static str,
    /// Only changes Claude Code's own terminal UI, which claude++ replaces.
    pub tui: bool,
    pub help: &'static str,
}

/// The panel's tabs: claude++'s own settings, then Claude Code's by topic.
pub fn tabs() -> impl Iterator<Item = &'static str> {
    std::iter::once("claude++").chain(TABS.iter().copied())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    Own(usize),
    Cc(usize),
}

impl Row {
    /// (name, help), what a search looks through.
    fn text(self) -> (String, &'static str) {
        match self {
            Row::Own(i) => (format!("{} {}", ITEMS[i].label, ITEMS[i].key), ITEMS[i].help),
            Row::Cc(i) => (KEYS[i].key.to_string(), KEYS[i].help),
        }
    }
}

pub struct Ui {
    pub tab: usize,
    pub sel: usize,
    /// What is typed in the search bar; non-empty searches every tab.
    pub query: String,
    /// The Claude Code settings file edits go to.
    pub scope: Scope,
    /// A value being typed; Enter saves it.
    pub edit: Option<String>,
    /// (message, is an error) after a save.
    pub note: Option<(String, bool)>,
    /// The settings files as last read, in [`Scope::ALL`] order.
    files: [Map<String, Value>; 3],
    cwd: String,
}

impl Ui {
    pub fn new(cwd: &str) -> Ui {
        let mut ui = Ui { tab: 0, sel: 0, query: String::new(), scope: Scope::User, edit: None, note: None, files: Default::default(), cwd: cwd.into() };
        ui.reload();
        ui
    }

    fn reload(&mut self) {
        for (i, s) in Scope::ALL.iter().enumerate() {
            self.files[i] = s.file(&self.cwd).and_then(|f| read_json(&f).ok()).unwrap_or_default();
        }
    }

    /// The rows shown: the tab's, or every match of the search, those whose name matches first.
    pub fn rows(&self) -> Vec<Row> {
        let words: Vec<String> = self.query.to_lowercase().split_whitespace().map(String::from).collect();
        if words.is_empty() {
            return match self.tab {
                0 => (0..ITEMS.len()).map(Row::Own).collect(),
                t => (0..KEYS.len()).filter(|i| KEYS[*i].tab == t).map(Row::Cc).collect(),
            };
        }
        let all = (0..ITEMS.len()).map(Row::Own).chain((0..KEYS.len()).map(Row::Cc));
        let mut hits: Vec<(bool, Row)> = all
            .filter_map(|r| {
                let (name, help) = r.text();
                let (name, help) = (name.to_lowercase(), help.to_lowercase());
                let in_name = words.iter().all(|w| name.contains(w.as_str()));
                (in_name || words.iter().all(|w| name.contains(w.as_str()) || help.contains(w.as_str()))).then_some((!in_name, r))
            })
            .collect();
        hits.sort_by_key(|h| h.0);
        hits.into_iter().map(|h| h.1).collect()
    }

    pub fn at(&self) -> Option<Row> {
        self.rows().get(self.sel).copied()
    }

    pub fn step(&mut self, delta: isize) {
        let n = self.rows().len().max(1) as isize;
        self.sel = (self.sel as isize + delta).rem_euclid(n) as usize;
    }

    pub fn switch_tab(&mut self, delta: isize) {
        self.tab = (self.tab as isize + delta).rem_euclid(tabs().count() as isize) as usize;
        (self.sel, self.query) = (0, String::new());
    }

    /// user → project → local → user.
    pub fn cycle_scope(&mut self) {
        self.scope = Scope::ALL[(self.scope_i() + 2) % 3];
    }

    fn scope_i(&self) -> usize {
        Scope::ALL.iter().position(|s| *s == self.scope).unwrap_or(2)
    }

    fn value(&self, key: &str) -> Option<&Value> {
        get_path(&self.files[self.scope_i()], key)
    }

    /// For a key with choices, the value `delta` steps to (`None`: unset, so the default applies); `None`
    /// for a key that is typed instead.
    pub fn cycled(&self, k: &CcKey, delta: isize) -> Option<Option<Value>> {
        let opts: Vec<Value> = match k.kind {
            Kind::Bool => vec![true.into(), false.into()],
            Kind::Choice(c) => c.iter().map(|s| Value::from(*s)).collect(),
            _ => return None,
        };
        // 0 is unset; a value that is not one of the choices counts as unset.
        let at = self.value(k.key).and_then(|v| opts.iter().position(|o| o == v)).map_or(0, |i| i + 1) as isize;
        let next = (at + delta).rem_euclid(opts.len() as isize + 1) as usize;
        Some(next.checked_sub(1).map(|i| opts[i].clone()))
    }

    /// The text a typed value starts from.
    pub fn edit_text(&self, k: &CcKey) -> String {
        match self.value(k.key) {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    }

    /// Write `v` (`None` removes the key) to the chosen file and read the files again.
    pub fn save(&mut self, k: &CcKey, v: Option<Value>) -> Result<(), String> {
        let file = self.scope.file(&self.cwd).ok_or("no settings location")?;
        let mut m = read_json(&file)?;
        if v.is_none() && get_path(&m, k.key).is_none() {
            return Ok(()); // nothing to remove; do not create the file
        }
        set_path(&mut m, k.key, v).map_err(|e| format!("{}: {e}", file.display()))?;
        write_json(&file, m)?;
        self.reload();
        Ok(())
    }
}

/// What typed text means for `k`: `None` (empty) removes the key.
pub fn parse(k: &CcKey, text: &str) -> Result<Option<Value>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match k.kind {
        Kind::Number => match t.parse::<i64>() {
            Ok(n) => Ok(Some(n.into())),
            Err(_) => t.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(|n| Some(Value::Number(n))).ok_or(format!("{t:?} is not a number")),
        },
        Kind::Json => serde_json::from_str(t).map(Some).map_err(|e| format!("not valid JSON: {e}")),
        _ => Ok(Some(Value::String(t.into()))),
    }
}

fn get_path<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    let mut parts = key.split('.');
    let mut v = m.get(parts.next()?)?;
    for p in parts {
        v = v.get(p)?;
    }
    Some(v)
}

fn set_path(m: &mut Map<String, Value>, key: &str, v: Option<Value>) -> Result<(), String> {
    let (parents, leaf) = key.rsplit_once('.').map_or((None, key), |(p, l)| (Some(p), l));
    let mut obj = m;
    for part in parents.into_iter().flat_map(|p| p.split('.')) {
        let next = obj.entry(part).or_insert_with(|| Value::Object(Map::new()));
        obj = next.as_object_mut().ok_or(format!("\"{part}\" is not an object"))?;
    }
    match v {
        Some(v) => obj.insert(leaf.into(), v),
        None => obj.remove(leaf),
    };
    Ok(())
}

/// The value cycle for claude++'s own setting: the value `delta` steps from `current` (wrapping). A current
/// value that is not one of the choices (set by a flag, say) steps onto the first or last choice.
pub fn cycle(item: &Item, current: &str, delta: isize) -> &'static str {
    let n = item.choices.len() as isize;
    let at = item.choices.iter().position(|c| c.0 == current).map_or(if delta > 0 { -1 } else { n }, |i| i as isize);
    item.choices[(at + delta).rem_euclid(n) as usize].0
}

fn short(v: &Value, max: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    if s.chars().count() <= max { s } else { s.chars().take(max.saturating_sub(1)).collect::<String>() + "…" }
}

/// The search bar and the tab strip, scrolled so the current tab shows.
fn head(ui: &Ui, n: usize, width: usize) -> Vec<Line<'static>> {
    let searching = !ui.query.is_empty();
    let mut chips: Vec<Span> = tabs().enumerate().map(|(i, t)| Span::styled(format!(" {t} "), if i == ui.tab && !searching { theme::selected() } else { theme::dim() })).collect();
    // Drop tabs off the left only while the current one would end past the edge.
    let upto = |c: &[Span], n: usize| c[..=n].iter().map(|s| s.content.chars().count() + 1).sum::<usize>() + 2;
    let mut skipped = 0;
    while skipped < ui.tab && upto(&chips, ui.tab - skipped) > width {
        chips.remove(0);
        skipped += 1;
    }
    let mut tabs_line = vec![Span::styled(if skipped > 0 { "‹ " } else { "  " }, theme::dim())];
    for c in chips {
        tabs_line.extend([c, Span::raw(" ")]);
    }
    let search = if searching {
        vec![Span::styled("⌕ ", theme::accent()), Span::raw(ui.query.clone()), Span::styled(" ", theme::selected()), Span::styled(format!("  {n} found · esc clears"), theme::dim())]
    } else {
        vec![Span::styled("⌕ type to search every setting", theme::dim())]
    };
    vec![Line::from(search), Line::from(tabs_line)]
}

/// `width` is the terminal width. The help pane keeps one height while the cursor moves within a tab.
pub fn view(ui: &Ui, width: usize, current: impl Fn(&str) -> String) -> View {
    let inner = width.saturating_sub(4).max(20);
    let rows = ui.rows();
    let sel = ui.sel.min(rows.len().saturating_sub(1));
    let own_w = ITEMS.iter().map(|i| i.label.len()).max().unwrap_or(0) + 2;
    let key_w = rows.iter().filter_map(|r| if let Row::Cc(i) = r { Some(KEYS[*i].key.len()) } else { None }).max().unwrap_or(0).min(inner / 2);
    let si = ui.scope_i();
    let mut body: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(n, row)| {
            let on = n == sel;
            let name_style = if on { theme::bold() } else { Default::default() };
            match *row {
                Row::Own(i) => {
                    let item = &ITEMS[i];
                    let value = current(item.key);
                    let mut spans = vec![Span::styled(format!("{:<own_w$}", item.label), name_style)];
                    for (v, label) in item.choices {
                        let active = *v == value;
                        spans.push(theme::chip(label, active, active && on, theme::bold()));
                    }
                    if !item.choices.iter().any(|c| c.0 == value) {
                        spans.push(theme::chip(&value, true, on, theme::bold())); // e.g. set by a flag
                    }
                    if !ui.query.is_empty() {
                        spans.push(Span::styled("  claude++", theme::dim()));
                    }
                    row_with(on, false, spans)
                }
                Row::Cc(i) => {
                    let k = &KEYS[i];
                    let room = inner.saturating_sub(key_w + 22).max(10);
                    let mut spans = vec![Span::styled(format!("{:<key_w$}  ", k.key), name_style)];
                    match (&ui.edit, on) {
                        (Some(text), true) => spans.extend([Span::raw(text.clone()), Span::styled(" ", theme::selected())]),
                        _ => match get_path(&ui.files[si], k.key) {
                            Some(v) => spans.push(Span::styled(short(v, room), if on { theme::selected() } else { theme::bold() })),
                            // Not in this file: what a more or less specific file says, else the default.
                            None => spans.push(match Scope::ALL.iter().enumerate().find_map(|(j, s)| get_path(&ui.files[j], k.key).map(|v| (s, v))) {
                                Some((s, v)) => Span::styled(format!("({}: {})", s.name(), short(v, room)), theme::dim()),
                                None => Span::styled("default", theme::dim()),
                            }),
                        },
                    }
                    if k.tui {
                        spans.push(Span::styled("  claude UI", theme::dim()));
                    }
                    if !ui.query.is_empty() {
                        spans.push(Span::styled(format!("  {}", TABS[k.tab - 1]), theme::dim()));
                    }
                    row_with(on, false, spans)
                }
            }
        })
        .collect();
    if body.is_empty() {
        body.push(Line::styled("  nothing matches; esc clears the search", theme::dim()));
    }

    let help_of = |r: &Row| -> String {
        match *r {
            Row::Own(i) => format!("{}  Saved in ~/.config/claude++/config.json as {}.", ITEMS[i].help, ITEMS[i].key),
            Row::Cc(i) => {
                let k = &KEYS[i];
                let what = match k.kind {
                    Kind::Bool => "←→: true, false or unset".to_string(),
                    Kind::Choice(c) => format!("←→: {} or unset", c.join(", ")),
                    Kind::Number => "enter: type a number (empty unsets)".into(),
                    Kind::Text => "enter: type the value (empty unsets)".into(),
                    Kind::Json => "enter: type it as JSON (empty unsets)".into(),
                };
                let mut h = format!("{}  {what}.", k.help);
                if k.scope != "Any file" {
                    h += &format!(" Read only from: {}.", k.scope.to_lowercase());
                }
                if k.tui {
                    h += " Only Claude Code's own terminal UI uses it; claude++ draws its own.";
                }
                h
            }
        }
    };
    const HELP_MAX: usize = 6;
    let help_rows = rows.iter().map(|r| wrap(&help_of(r), inner).len()).max().unwrap_or(1).min(HELP_MAX);
    let mut help = rows.get(sel).map(|r| wrap(&theme::keys(&help_of(r)), inner)).unwrap_or_default();
    if help.len() > help_rows {
        help.truncate(help_rows);
        if let Some(l) = help.last_mut() {
            l.push('…');
        }
    }
    help.resize(help_rows, String::new());
    let mut tail = vec![Line::raw("")];
    tail.extend(help.into_iter().map(|l| Line::styled(l, theme::dim())));
    let file = format!("saves to {} settings ({})", ui.scope.name(), match ui.scope { Scope::User => "~/.claude/settings.json", Scope::Project => ".claude/settings.json", Scope::Local => ".claude/settings.local.json" });
    tail.push(match &ui.note {
        Some((n, true)) => Line::styled(n.clone(), theme::danger()),
        Some((n, false)) => Line::styled(n.clone(), theme::ok()),
        None if ui.tab > 0 || !ui.query.is_empty() => Line::styled(format!("Claude Code {file} · ctrl+s changes"), theme::dim()),
        None => Line::raw(""),
    });
    let footer = if ui.edit.is_some() {
        theme::hints(&[("enter", "save"), ("esc", "cancel")])
    } else {
        theme::hints(&[("↑↓", "move"), ("←→", "change"), ("enter", "edit"), ("tab", "section"), ("del", "unset"), ("ctrl+s", "file"), ("esc", "close")])
    };
    View::new("Config", body, sel, footer).with_head(head(ui, rows.len(), inner)).with_tail(tail).rows(12, 16)
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

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn temp_ui(tag: &str) -> (Ui, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-cfgui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ui = Ui::new(dir.to_str().unwrap());
        ui.scope = Scope::Local; // never the real ~/.claude/settings.json
        (ui, dir)
    }

    fn key(name: &str) -> &'static CcKey {
        KEYS.iter().find(|k| k.key == name).unwrap()
    }

    #[test]
    fn the_catalog_is_sane() {
        let mut names: Vec<_> = KEYS.iter().map(|k| k.key).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), KEYS.len(), "keys are unique");
        assert!(KEYS.len() > 150, "every user-settable key: {}", KEYS.len());
        assert!(KEYS.iter().all(|k| k.tab >= 1 && k.tab <= TABS.len() && !k.help.is_empty()));
        assert!((1..=TABS.len()).all(|t| KEYS.iter().any(|k| k.tab == t)), "no empty section");
        assert!(matches!(key("permissions.defaultMode").kind, Kind::Choice(c) if c.contains(&"plan")));
        assert!(matches!(key("autoCompactEnabled").kind, Kind::Bool));
        assert!(!KEYS.iter().any(|k| k.key == "sandbox"), "a parent whose fields are listed is left out");
    }

    #[test]
    fn tabs_search_and_the_view() {
        let (mut ui, _d) = temp_ui("view");
        assert!(ui.rows().iter().all(|r| matches!(r, Row::Own(_))), "claude++ first");
        ui.switch_tab(-1);
        assert_eq!(ui.tab, TABS.len(), "wraps to the last section");
        ui.switch_tab(1);
        ui.query = "compact".into();
        let rows = ui.rows();
        assert!(rows.contains(&Row::Cc(KEYS.iter().position(|k| k.key == "autoCompactEnabled").unwrap())), "search spans every tab");
        assert!(matches!(rows[0], Row::Cc(i) if KEYS[i].key.to_lowercase().contains("compact")), "name matches come first");
        ui.query = "zzzz qqqq".into();
        let v = view(&ui, 100, |_| "x".into());
        assert!(text(&v.body[0]).contains("nothing matches"));
        ui.query.clear();
        ui.tab = 1;
        for w in [60, 100, 200] {
            let heights: Vec<usize> = (0..ui.rows().len()).map(|n| { ui.sel = n; view(&ui, w, |_| "x".into()).tail.len() }).collect();
            assert!(heights.windows(2).all(|p| p[0] == p[1]), "width {w}: the help keeps one height");
            let v = view(&ui, w, |_| "x".into());
            assert!(text(&v.head[1]).contains(" Model "), "width {w}: the current tab is on screen");
        }
        ui.tab = TABS.len();
        assert!(text(&view(&ui, 60, |_| "x".into()).head[1]).contains(TABS[TABS.len() - 1]), "a narrow strip scrolls to the last tab");
    }

    #[test]
    fn choices_cycle_through_unset_and_values_save_nested() {
        let (mut ui, dir) = temp_ui("save");
        let file = dir.join(".claude/settings.local.json");
        let k = key("sandbox.enabled");
        assert_eq!(ui.cycled(k, 1), Some(Some(Value::Bool(true))));
        ui.save(k, Some(true.into())).unwrap();
        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(on_disk["sandbox"]["enabled"], true, "a dotted key is a field of an object");
        assert_eq!(ui.cycled(k, 1), Some(Some(Value::Bool(false))));
        assert_eq!(ui.cycled(k, -1), Some(None), "unset is one of the steps");
        ui.save(k, None).unwrap();
        assert!(get_path(&ui.files[0], "sandbox.enabled").is_none());
        assert!(ui.cycled(key("model"), 1).is_none(), "a text key is typed, not cycled");

        let n = key("cleanupPeriodDays");
        assert_eq!(parse(n, "30"), Ok(Some(30.into())));
        assert!(parse(n, "soon").is_err());
        assert_eq!(parse(key("permissions.allow"), r#"["Bash(ls)"]"#).unwrap(), Some(serde_json::json!(["Bash(ls)"])));
        assert!(parse(key("permissions.allow"), "[oops").is_err());
        assert_eq!(parse(key("model"), "  "), Ok(None), "empty unsets");
        ui.save(key("model"), parse(key("model"), "opus").unwrap()).unwrap();
        assert_eq!(ui.edit_text(key("model")), "opus", "strings edit without quotes");
        ui.tab = 1;
        let v = view(&ui, 120, |_| "x".into());
        assert!(v.tail.iter().any(|l| text(l).contains("local settings")), "the footer names the file");

        std::fs::write(&file, r#"{"sandbox": 3}"#).unwrap();
        ui.reload();
        assert!(ui.save(k, Some(true.into())).is_err(), "never clobbers a non-object");
        std::fs::write(&file, "not json").unwrap();
        assert!(ui.save(k, Some(true.into())).is_err(), "nor a broken file");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json");
        ui.save(key("model"), None).unwrap_err();
    }
}
