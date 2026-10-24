//! Saved permission rules (`/permissions`), as in Claude Code: the `permissions.allow`, `ask` and `deny`
//! lists of your user (`~/.claude/settings.json`), project (`.claude/settings.json`) and local
//! (`.claude/settings.local.json`) settings, shown by behavior with the file each rule comes from, and
//! added to or deleted from the file you pick. Only the `permissions` list being changed is touched: the
//! rest of the file keeps its keys, order and values (it is rewritten with Claude Code's own 2-space
//! style), a file that is not valid JSON is never overwritten, and a symlinked file is edited in place.
//! Running sessions pick the change up by themselves (Claude Code watches its settings files).
//! Deny beats ask beats allow, whichever file a rule is in.

use crate::panel::{View, row_with};
use crate::session::Session;
use crate::theme;
use crate::ui::wrap;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Behavior {
    Allow,
    Ask,
    Deny,
}

impl Behavior {
    pub const ALL: [Behavior; 3] = [Behavior::Allow, Behavior::Ask, Behavior::Deny];
    pub fn key(self) -> &'static str {
        match self { Behavior::Allow => "allow", Behavior::Ask => "ask", Behavior::Deny => "deny" }
    }
    fn label(self) -> &'static str {
        match self { Behavior::Allow => "Allow", Behavior::Ask => "Ask", Behavior::Deny => "Deny" }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    User,
    Project,
    Local,
}

impl Scope {
    pub const ALL: [Scope; 3] = [Scope::Local, Scope::Project, Scope::User];
    pub fn name(self) -> &'static str {
        match self { Scope::User => "user", Scope::Project => "project", Scope::Local => "local" }
    }
    fn blurb(self) -> &'static str {
        match self {
            Scope::Local => "this project, only you (.claude/settings.local.json)",
            Scope::Project => "this project, shared with the repo (.claude/settings.json)",
            Scope::User => "every project (~/.claude/settings.json)",
        }
    }
    pub fn file(self, cwd: &str) -> Option<PathBuf> {
        match self {
            Scope::User => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/settings.json")),
            Scope::Project => Some(Path::new(cwd).join(".claude/settings.json")),
            Scope::Local => Some(Path::new(cwd).join(".claude/settings.local.json")),
        }
    }
}

/// Where a listed rule comes from: a settings file, or this tab's own `/session` rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    File(Scope),
    Tab,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Rule {
    pub behavior: Behavior,
    pub text: String,
    pub source: Source,
}

fn read_json(file: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {e}", file.display())),
        Ok(t) if t.trim().is_empty() => Ok(Map::new()),
        Ok(t) => match serde_json::from_str::<Value>(&t) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(format!("{} is not a JSON object; fix it by hand first, nothing was changed", file.display())),
        },
    }
}

fn rules_in(m: &Map<String, Value>, b: Behavior) -> Vec<String> {
    m.get("permissions").and_then(|p| p.get(b.key())).and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect()
}

/// Every saved rule for `cwd`, plus the tab's own (`tab`: name and whether it allows).
pub fn load(cwd: &str, tab: &[(String, bool)]) -> Vec<Rule> {
    let mut out = vec![];
    for scope in Scope::ALL {
        let Some(m) = scope.file(cwd).and_then(|f| read_json(&f).ok()) else { continue };
        for b in Behavior::ALL {
            out.extend(rules_in(&m, b).into_iter().map(|text| Rule { behavior: b, text, source: Source::File(scope) }));
        }
    }
    out.extend(tab.iter().map(|(t, allow)| Rule { behavior: if *allow { Behavior::Allow } else { Behavior::Deny }, text: t.clone(), source: Source::Tab }));
    out
}

fn write_json(file: &Path, m: Map<String, Value>) -> Result<(), String> {
    let target = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(m)).map_err(|e| e.to_string())? + "\n";
    let tmp = target.with_extension(format!("json.claudepp-{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Add `rule` to the file for `scope` (no duplicates). Returns false when it was there already.
pub fn add(file: &Path, b: Behavior, rule: &str) -> Result<bool, String> {
    let mut m = read_json(file)?;
    if rules_in(&m, b).iter().any(|r| r == rule) {
        return Ok(false);
    }
    let perms = m.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
    let Some(perms) = perms.as_object_mut() else { return Err(format!("{}: \"permissions\" is not an object", file.display())) };
    let list = perms.entry(b.key()).or_insert_with(|| Value::Array(vec![]));
    let Some(list) = list.as_array_mut() else { return Err(format!("{}: \"permissions.{}\" is not a list", file.display(), b.key())) };
    list.push(Value::String(rule.into()));
    write_json(file, m).map(|()| true)
}

/// Remove `rule` from the file. Leaves an emptied list in place (as Claude Code does) and never creates files.
pub fn remove(file: &Path, b: Behavior, rule: &str) -> Result<bool, String> {
    let mut m = read_json(file)?;
    let Some(list) = m.get_mut("permissions").and_then(|p| p.get_mut(b.key())).and_then(Value::as_array_mut) else { return Ok(false) };
    let before = list.len();
    list.retain(|v| v.as_str() != Some(rule));
    if list.len() == before {
        return Ok(false);
    }
    write_json(file, m).map(|()| true)
}

#[derive(Clone, Default)]
pub enum Stage {
    #[default]
    List,
    /// Typing the rule.
    Typing(String),
    /// Choosing the file for this rule; the index into [`Scope::ALL`].
    Where(String, usize),
    /// A delete waiting for a second press: the rule it is for.
    Confirm(usize),
}

#[derive(Clone)]
pub struct Ui {
    pub tab: Behavior,
    pub sel: usize,
    pub stage: Stage,
    pub rules: Vec<Rule>,
    pub cwd: String,
    /// The last write failed, or says what happened.
    pub note: Option<String>,
}

impl Ui {
    pub fn new(cwd: &str, s: &Session) -> Ui {
        Ui { tab: Behavior::Allow, sel: 0, stage: Stage::List, rules: load(cwd, &s.rules), cwd: cwd.into(), note: None }
    }

    pub fn reload(&mut self, s: &Session) {
        self.rules = load(&self.cwd, &s.rules);
        self.sel = self.sel.min(self.shown().len());
    }

    /// Rules on the current tab (row 0 of the list is the "add" row, so rule `i` is row `i + 1`).
    pub fn shown(&self) -> Vec<&Rule> {
        self.rules.iter().filter(|r| r.behavior == self.tab).collect()
    }

    /// The rule at the cursor, if it is on a rule row.
    pub fn at(&self) -> Option<&Rule> {
        self.sel.checked_sub(1).and_then(|i| self.shown().get(i).copied())
    }

    pub fn switch(&mut self, delta: isize) {
        let i = Behavior::ALL.iter().position(|b| *b == self.tab).unwrap_or(0) as isize;
        self.tab = Behavior::ALL[(i + delta).rem_euclid(3) as usize];
        self.sel = 0;
        self.stage = Stage::List;
    }

    /// A stronger behavior that holds the same rule text, if any (deny > ask > allow).
    fn overridden_by(&self, r: &Rule) -> Option<Behavior> {
        let rank = |b: Behavior| match b { Behavior::Allow => 0, Behavior::Ask => 1, Behavior::Deny => 2 };
        self.rules.iter().filter(|o| o.text == r.text && rank(o.behavior) > rank(r.behavior)).map(|o| o.behavior).max_by_key(|b| rank(*b))
    }
}

pub fn view(ui: &Ui, width: usize) -> View {
    let head = vec![Line::from(
        Behavior::ALL
            .iter()
            .flat_map(|b| {
                let n = ui.rules.iter().filter(|r| r.behavior == *b).count();
                [Span::styled(format!(" {} {n} ", b.label()), if *b == ui.tab { theme::selected() } else { theme::dim() }), Span::raw(" ")]
            })
            .collect::<Vec<_>>(),
    )];
    let shown = ui.shown();
    let sel = ui.sel.min(shown.len());
    let mut body: Vec<Line> = vec![];
    body.push(match &ui.stage {
        Stage::Typing(text) => row_with(true, false, vec![Span::styled("rule ", theme::dim()), Span::raw(text.clone()), Span::styled(" ", theme::selected())]),
        _ => row_with(sel == 0, false, vec![Span::styled("+ add a new rule…", if sel == 0 { theme::bold() } else { theme::dim() })]),
    });
    for (i, r) in shown.iter().enumerate() {
        let on = sel == i + 1;
        let over = ui.overridden_by(r);
        let src = match r.source { Source::File(s) => s.name(), Source::Tab => "this tab" };
        let confirm = matches!(ui.stage, Stage::Confirm(n) if n == i);
        let mut v = vec![Span::styled(r.text.clone(), if over.is_some() { theme::dim() } else if on { theme::bold() } else { Style::new() }), Span::raw("  "), Span::styled(src, theme::dim())];
        if let Some(b) = over {
            v.push(Span::styled(format!("  overridden by {}", b.key()), theme::dim()));
        }
        if confirm {
            v.push(Span::styled("  press d again to delete", theme::danger()));
        }
        body.push(row_with(on, false, v));
    }
    let mut tail = vec![Line::raw("")];
    let help = match &ui.stage {
        Stage::Where(rule, w) => {
            tail.push(Line::from(vec![Span::styled("save ", theme::dim()), Span::styled(rule.clone(), theme::bold()), Span::styled(format!(" as {} in:", ui.tab.key()), theme::dim())]));
            for (i, s) in Scope::ALL.iter().enumerate() {
                tail.push(row_with(i == *w, false, vec![Span::styled(format!("{:<8}", s.name()), if i == *w { theme::bold() } else { Style::new() }), Span::styled(s.blurb(), theme::dim())]));
            }
            String::new()
        }
        _ => match (sel, shown.get(sel.wrapping_sub(1))) {
            (0, _) => "Enter, then type a rule such as Bash(git push:*), Read(./secrets/**) or mcp__server__tool. Deny beats ask beats allow, in any file.".to_string(),
            (_, Some(r)) if r.source == Source::Tab => "This tab's own rule; change it in /session. It is not saved to any file.".to_string(),
            _ => "d deletes this rule from its file. Running sessions pick the change up by themselves.".to_string(),
        },
    };
    tail.extend(wrap(&help, width.saturating_sub(4).max(20)).into_iter().filter(|l| !l.is_empty()).map(|l| Line::styled(l, theme::dim())));
    if let Some(n) = &ui.note {
        tail.push(Line::styled(n.clone(), theme::danger()));
    }
    let footer = match ui.stage {
        Stage::Typing(_) => theme::hints(&[("enter", "next"), ("esc", "cancel")]),
        Stage::Where(..) => theme::hints(&[("↑↓", "file"), ("enter", "save"), ("esc", "back")]),
        _ => theme::hints(&[("↑↓", "move"), ("←→", "allow/ask/deny"), ("d", "delete"), ("esc", "close")]),
    };
    View::new("Permissions", body, sel, footer).with_head(head).with_tail(tail).rows(0, 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("claudeplusplus-perm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn adding_and_removing_touch_only_that_list_and_keep_the_rest_byte_for_byte() {
        let d = temp("edit");
        let f = d.join("settings.json");
        let before = "{\n  \"env\": {\n    \"Z\": \"é\"\n  },\n  \"permissions\": {\n    \"allow\": [\n      \"Read\"\n    ],\n    \"defaultMode\": \"auto\"\n  },\n  \"hooks\": {\n    \"A\": [\n      1,\n      2\n    ]\n  }\n}\n";
        std::fs::write(&f, before).unwrap();
        assert_eq!(add(&f, Behavior::Allow, "Read"), Ok(false), "no duplicates");
        assert_eq!(add(&f, Behavior::Deny, "Bash(rm:*)"), Ok(true));
        let after = std::fs::read_to_string(&f).unwrap();
        assert_eq!(after, before.replace("\"defaultMode\": \"auto\"\n", "\"defaultMode\": \"auto\",\n    \"deny\": [\n      \"Bash(rm:*)\"\n    ]\n"), "{after}");
        assert_eq!(remove(&f, Behavior::Deny, "Bash(rm:*)"), Ok(true));
        assert_eq!(remove(&f, Behavior::Deny, "Bash(rm:*)"), Ok(false));
        assert!(std::fs::read_to_string(&f).unwrap().contains("\"deny\": []"));
    }

    #[test]
    fn a_new_file_is_created_and_a_broken_one_is_never_overwritten() {
        let d = temp("new");
        let f = d.join(".claude/settings.local.json");
        assert_eq!(add(&f, Behavior::Ask, "WebFetch"), Ok(true));
        assert_eq!(read_json(&f).unwrap()["permissions"]["ask"][0], "WebFetch");
        assert_eq!(remove(&d.join("none.json"), Behavior::Ask, "x"), Ok(false));
        assert!(!d.join("none.json").exists(), "removing never creates a file");
        std::fs::write(&f, "{ not json").unwrap();
        assert!(add(&f, Behavior::Allow, "Read").unwrap_err().contains("nothing was changed"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{ not json");
    }

    #[test]
    fn load_groups_by_file_and_marks_what_a_stronger_behavior_overrides() {
        let d = temp("load");
        let cwd = d.to_str().unwrap();
        add(&Scope::Local.file(cwd).unwrap(), Behavior::Allow, "Bash(git push:*)").unwrap();
        add(&Scope::Project.file(cwd).unwrap(), Behavior::Deny, "Bash(git push:*)").unwrap();
        let rules = load(cwd, &[("Read".into(), true)]);
        assert!(rules.iter().any(|r| r.source == Source::Tab && r.text == "Read"));
        let ui = Ui { tab: Behavior::Allow, sel: 0, stage: Stage::List, rules, cwd: cwd.into(), note: None };
        let allow = ui.rules.iter().find(|r| r.behavior == Behavior::Allow && r.source == Source::File(Scope::Local)).unwrap();
        assert_eq!(ui.overridden_by(allow), Some(Behavior::Deny));
        let v = view(&ui, 100);
        let text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert!(v.body.iter().any(|l| text(l).contains("overridden by deny")));
        assert!(text(&v.head[0]).contains("Deny 1"));
    }

    #[test]
    #[ignore = "reads the real ~/.claude/settings.json"]
    fn the_real_user_file_round_trips_unchanged() {
        let f = Scope::User.file("").unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        let m = read_json(&f).unwrap();
        assert_eq!(serde_json::to_string_pretty(&Value::Object(m)).unwrap() + "\n", text);
    }
}
