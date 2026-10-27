//! `/optimize`: trims what a new session loads, for one project, from evidence instead of guesses. It reads
//! what this project's past sessions actually used (tools, skills) and how much each instruction section
//! has to do with the project, proposes changes at a chosen level, and saves the ticked ones to
//! `<project>/.claude++/optimize.json`. Nothing it saves edits a file of yours, and `/optimize off` deletes it.
//!
//! What a change does, all through Claude Code's own settings and flags (no model call):
//! - skills: `skillListingMaxDescChars` caps every description; `skillOverrides` lists a skill by name only
//!   (`name-only`) or hides it from the model (`off`); `/name` keeps working for the first.
//! - instructions: a section is cut to its heading and first line (the model is told where the rest is),
//!   or hidden; see `instructions.rs`.
//! - tools (aggressive only): `--tools` is narrowed to the core set plus what the project used, and MCP
//!   servers are left out when none were ever called.
//!
//! A conversation already under way keeps the prompt it started with; the panel says when a change needs a
//! re-render of it (which re-caches the context once).

use crate::instructions::{self, BRIEF, Rules, Section};
use crate::panel::{View, row_with};
use crate::theme;
use crate::ui::wrap;
use ratatui::text::{Line, Span};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Light,
    Balanced,
    Aggressive,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Light, Level::Balanced, Level::Aggressive];
    pub fn name(self) -> &'static str {
        match self { Level::Light => "light", Level::Balanced => "balanced", Level::Aggressive => "aggressive" }
    }
    pub fn parse(s: &str) -> Option<Level> {
        Level::ALL.into_iter().find(|l| l.name() == s)
    }
    fn blurb(self) -> &'static str {
        match self {
            Level::Light => "shorter skill descriptions only; nothing is hidden",
            Level::Balanced => "also: skills this project never used are listed by name only, unrelated instruction sections are shortened",
            Level::Aggressive => "also: unused skills are hidden, strongly unrelated sections dropped, tools narrowed to what the project used",
        }
    }
    fn cap(self) -> usize {
        match self { Level::Light => 160, Level::Balanced => 120, Level::Aggressive => 60 }
    }
}

/// Skills and tools below/above these are never touched or always kept.
const CORE_TOOLS: [&str; 10] = ["Bash", "Read", "Edit", "Write", "Glob", "Grep", "Agent", "AskUserQuestion", "WebFetch", "WebSearch"];
/// What a skill costs in the listing when shown by name only (measured: a name and a few words).
const NAME_ONLY_TOKENS: u64 = 12;
/// Fewer past sessions than this and per-skill "never used" proves nothing.
const MIN_SESSIONS: usize = 3;
const BYTES_PER_TOKEN: usize = 3;

#[derive(Default, Clone, Debug)]
pub struct Evidence {
    pub sessions: usize,
    pub tools: BTreeMap<String, usize>,
    pub skills: BTreeMap<String, usize>,
    pub vocab: BTreeSet<String>,
    /// Stopped early (time or size budget): counts are lower bounds.
    pub partial: bool,
}

const STOP: [&str; 40] = [
    "about", "after", "again", "also", "always", "because", "before", "being", "between", "build", "check", "could", "every", "first", "should", "their", "there", "these", "those", "through", "under", "until", "using", "which", "while", "where", "with", "without", "would", "other", "never", "model", "output", "input", "files", "file", "command", "commands", "default", "value",
];

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 5).map(|w| w.to_lowercase()).filter(|w| !STOP.contains(&w.as_str()))
}

/// Read a project's past sessions and files. Bounded: at most `max_files` transcripts and `budget` of wall time.
pub fn scan(cwd: &str, max_files: usize, budget: std::time::Duration) -> Evidence {
    use std::io::BufRead;
    let started = std::time::Instant::now();
    let mut ev = Evidence::default();
    // What the project is about: its file names and top-level docs.
    let mut stack = vec![(PathBuf::from(cwd), 0)];
    let mut seen = 0;
    while let Some((dir, depth)) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || ["node_modules", "target", "dist", "build", "venv", "__pycache__"].contains(&name.as_str()) || seen > 4000 {
                continue;
            }
            seen += 1;
            ev.vocab.extend(words(&name));
            if e.path().is_dir() && depth < 3 {
                stack.push((e.path(), depth + 1));
            }
        }
    }
    for doc in ["README.md", "README", "Cargo.toml", "package.json", "pyproject.toml", "go.mod"] {
        if let Ok(t) = std::fs::read_to_string(Path::new(cwd).join(doc)) {
            ev.vocab.extend(words(&t.chars().take(6000).collect::<String>()));
        }
    }
    let mut prompts = 0;
    for file in crate::history::transcripts(cwd, max_files) {
        if started.elapsed() > budget {
            ev.partial = true;
            break;
        }
        let Ok(f) = std::fs::File::open(&file) else { continue };
        ev.sessions += 1;
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if started.elapsed() > budget {
                ev.partial = true;
                break;
            }
            let has_tool = line.contains("\"tool_use\"");
            let has_cmd = line.contains("<command-name>");
            let is_user = prompts < 300 && line.contains("\"type\":\"user\"");
            if !(has_tool || has_cmd || is_user) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if is_user && let Some(t) = crate::history::user_text(&v) {
                prompts += 1;
                ev.vocab.extend(words(&t.chars().take(400).collect::<String>()));
            }
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_use" && let Some(n) = b["name"].as_str() {
                    *ev.tools.entry(n.to_string()).or_default() += 1;
                    if n == "Skill" && let Some(k) = b["input"]["skill"].as_str() {
                        *ev.skills.entry(k.to_string()).or_default() += 1;
                    }
                }
            }
            if has_cmd && let Some(t) = v["message"]["content"].as_str() && let Some(c) = t.split("<command-name>").nth(1).and_then(|r| r.split("</command-name>").next()) {
                *ev.skills.entry(c.trim_start_matches('/').to_string()).or_default() += 1;
            }
        }
    }
    ev
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Cap(usize),
    Skill(String, &'static str),
    /// file, section id (with the `~` prefix when brief)
    Section(String, String),
    Tools(Vec<String>),
    StrictMcp,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub group: &'static str,
    pub label: String,
    pub why: String,
    pub saves: u64,
    pub on: bool,
    pub change: Change,
}

/// How much of a section's distinctive words the project's own vocabulary contains (0 to 1).
pub fn relevance(text: &str, sec: &Section, vocab: &BTreeSet<String>) -> Option<f64> {
    let body: String = text.lines().skip(sec.from).take(sec.to.saturating_sub(sec.from).min(40)).collect::<Vec<_>>().join(" ");
    let w: BTreeSet<String> = words(&body.chars().take(900).collect::<String>()).collect();
    (w.len() >= 6).then(|| w.iter().filter(|x| vocab.contains(*x)).count() as f64 / w.len() as f64)
}

pub struct Input<'a> {
    pub level: Level,
    pub ev: &'a Evidence,
    pub skills: &'a [(String, u64)],
    pub files: &'a [instructions::Source],
}

pub fn propose(i: &Input) -> Vec<Item> {
    let mut out = vec![];
    let cap = i.level.cap();
    let cap_tokens = cap as u64 / 3 + NAME_ONLY_TOKENS;
    let trusted = i.ev.sessions >= MIN_SESSIONS;
    let change_for = |name: &str, tokens: u64| -> Option<(&'static str, u64)> {
        if !trusted || tokens < 15 || i.ev.skills.contains_key(name) || i.ev.skills.keys().any(|k| k.ends_with(&format!(":{name}"))) {
            return None;
        }
        match i.level {
            Level::Light => None,
            Level::Balanced => Some(("name-only", tokens.saturating_sub(NAME_ONLY_TOKENS))),
            Level::Aggressive => Some(("off", tokens)),
        }
    };
    let mut cap_saves = 0;
    let mut changed: Vec<(String, &'static str, u64)> = vec![];
    for (name, tokens) in i.skills {
        match change_for(name, *tokens) {
            Some((mode, saves)) => changed.push((name.clone(), mode, saves)),
            None => cap_saves += tokens.saturating_sub(cap_tokens),
        }
    }
    out.push(Item { group: "skills", label: format!("cap skill descriptions at {cap} characters"), why: format!("{} skills listed", i.skills.len()), saves: cap_saves, on: true, change: Change::Cap(cap) });
    changed.sort_by_key(|c| std::cmp::Reverse(c.2));
    for (name, mode, saves) in changed {
        let why = format!("never used in {} past sessions here; {}", i.ev.sessions, if mode == "off" { "hidden from the model (/name still works only for name-only)" } else { "still listed by name, /name works" });
        out.push(Item { group: "skills", label: format!("{name}  → {mode}"), why, saves, on: true, change: Change::Skill(name, mode) });
    }
    if i.level != Level::Light {
        for src in i.files {
            let mut taken: Vec<(usize, usize)> = vec![];
            for sec in &src.secs {
                if sec.bytes < 1500 || taken.iter().any(|(a, b)| sec.from >= *a && sec.to <= *b) {
                    continue;
                }
                let Some(score) = relevance(&src.text, sec, &i.ev.vocab) else { continue };
                let hide = i.level == Level::Aggressive && score < 0.12 && sec.bytes >= 3000;
                let brief = score < if i.level == Level::Aggressive { 0.3 } else { 0.22 } && sec.bytes >= if i.level == Level::Aggressive { 1500 } else { 3000 };
                if !(hide || brief) {
                    continue;
                }
                taken.push((sec.from, sec.to));
                let saves = (sec.bytes / BYTES_PER_TOKEN) as u64 - if hide { 0 } else { 60 };
                let id = if hide { sec.id.clone() } else { format!("{BRIEF}{}", sec.id) };
                let name = Path::new(&src.path).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
                out.push(Item {
                    group: "instructions",
                    label: format!("{} {}  ({name})", if hide { "hide" } else { "shorten" }, sec.title),
                    why: format!("{:.0}% of its key words appear in this project", score * 100.0),
                    saves,
                    on: true,
                    change: Change::Section(src.path.clone(), id),
                });
            }
        }
    }
    if i.level == Level::Aggressive && trusted {
        let mut tools: BTreeSet<String> = CORE_TOOLS.iter().map(|s| s.to_string()).collect();
        tools.extend(i.ev.tools.keys().filter(|t| t.chars().next().is_some_and(char::is_uppercase) && !t.starts_with("mcp__") && t.as_str() != "Skill").cloned());
        out.push(Item { group: "tools", label: format!("limit built-in tools to {}", tools.iter().cloned().collect::<Vec<_>>().join(", ")), why: "the core set plus every tool this project used".into(), saves: 0, on: true, change: Change::Tools(tools.into_iter().collect()) });
        if !i.ev.tools.keys().any(|t| t.starts_with("mcp__")) {
            out.push(Item { group: "tools", label: "leave MCP servers out".into(), why: "no MCP tool was ever called here".into(), saves: 0, on: true, change: Change::StrictMcp });
        }
    }
    out
}

/// What is saved for a project.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Saved {
    pub level: Option<Level>,
    pub cap: Option<usize>,
    pub skills: BTreeMap<String, String>,
    pub tools: Option<Vec<String>>,
    pub strict_mcp: bool,
    pub instructions: Rules,
}

pub fn file(cwd: &str) -> PathBuf {
    Path::new(cwd).join(".claude++/optimize.json")
}

pub fn from_items(level: Level, items: &[Item]) -> Saved {
    let mut s = Saved { level: Some(level), ..Default::default() };
    for it in items.iter().filter(|i| i.on) {
        match &it.change {
            Change::Cap(n) => s.cap = Some(*n),
            Change::Skill(n, m) => drop(s.skills.insert(n.clone(), m.to_string())),
            Change::Section(f, id) => drop(s.instructions.entry(f.clone()).or_default().insert(id.clone())),
            Change::Tools(t) => s.tools = Some(t.clone()),
            Change::StrictMcp => s.strict_mcp = true,
        }
    }
    s
}

pub fn save(cwd: &str, s: &Saved) -> std::io::Result<()> {
    let f = file(cwd);
    if let Some(d) = f.parent() {
        std::fs::create_dir_all(d)?;
    }
    let v = json!({
        "level": s.level.map(Level::name),
        "skillListingMaxDescChars": s.cap,
        "skillOverrides": s.skills,
        "tools": s.tools,
        "strict_mcp": s.strict_mcp,
        "instructions": s.instructions,
    });
    crate::config::write_atomic(&f, &(serde_json::to_string_pretty(&v)? + "\n"))
}

pub fn load(cwd: &str) -> Option<Saved> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(file(cwd)).ok()?).ok()?;
    Some(Saved {
        level: v["level"].as_str().and_then(Level::parse),
        cap: v["skillListingMaxDescChars"].as_u64().map(|n| n as usize),
        skills: v["skillOverrides"].as_object().into_iter().flatten().filter_map(|(k, x)| Some((k.clone(), x.as_str()?.to_string()))).collect(),
        tools: v["tools"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
        strict_mcp: v["strict_mcp"].as_bool().unwrap_or(false),
        instructions: v["instructions"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, x)| (k.clone(), x.as_array().into_iter().flatten().filter_map(|y| y.as_str().map(String::from)).collect::<BTreeSet<_>>()))
            .collect(),
    })
}

pub fn clear(cwd: &str) -> bool {
    std::fs::remove_file(file(cwd)).is_ok()
}

impl Saved {
    /// Put this project's settings and flags onto a launch. A profile that already chose tools keeps them.
    pub fn apply(&self, settings: &mut Map<String, Value>, args: &mut Vec<String>) {
        if let Some(n) = self.cap {
            settings.insert("skillListingMaxDescChars".into(), json!(n));
        }
        if !self.skills.is_empty() {
            settings.insert("skillOverrides".into(), json!(self.skills));
        }
        if let Some(t) = &self.tools && !args.iter().any(|a| a == "--tools") {
            args.extend(["--tools".into(), t.join(",")]);
        }
        if self.strict_mcp && !args.iter().any(|a| a == "--strict-mcp-config") {
            args.push("--strict-mcp-config".into());
        }
    }
}

// ---- the panel ----

pub type Shared = Arc<Mutex<Option<Evidence>>>;

pub struct Ui {
    pub level: Level,
    pub sel: usize,
    pub items: Vec<Item>,
    /// Waiting for the history scan or the skill listing.
    pub loading: bool,
    pub ev: Option<Evidence>,
    pub scan: Shared,
    pub skills: Option<Vec<(String, u64)>>,
    pub files: Vec<instructions::Source>,
    pub start_tokens: u64,
}

impl Ui {
    pub fn open(cwd: &str, level: Level) -> Ui {
        let scan: Shared = Arc::default();
        let (out, dir) = (scan.clone(), cwd.to_string());
        std::thread::spawn(move || {
            let ev = scan_budgeted(&dir);
            *out.lock().unwrap() = Some(ev);
        });
        let files = instructions::Ui::new(cwd, Rules::new()).files;
        Ui { level, sel: 0, items: vec![], loading: true, ev: None, scan, skills: None, files, start_tokens: 0 }
    }

    /// Take the scan result and the skill listing when both are in; true when the proposal was just built.
    pub fn poll(&mut self, skills: Option<&[(String, u64)]>, start_tokens: u64) -> bool {
        if !self.loading {
            return false;
        }
        if self.ev.is_none() {
            self.ev = self.scan.lock().unwrap().take();
        }
        if self.skills.is_none() {
            self.skills = skills.map(<[_]>::to_vec);
        }
        if self.ev.is_some() && self.skills.is_some() {
            self.start_tokens = start_tokens;
            self.loading = false;
            self.rebuild();
            return true;
        }
        false
    }

    pub fn rebuild(&mut self) {
        let (Some(ev), Some(skills)) = (&self.ev, &self.skills) else { return };
        self.items = propose(&Input { level: self.level, ev, skills, files: &self.files });
        self.sel = 0;
    }

    pub fn step_level(&mut self, delta: isize) {
        let i = Level::ALL.iter().position(|l| *l == self.level).unwrap_or(0) as isize;
        self.level = Level::ALL[(i + delta).rem_euclid(3) as usize];
        self.rebuild();
    }

    pub fn saves(&self) -> u64 {
        self.items.iter().filter(|i| i.on).map(|i| i.saves).sum()
    }

    pub fn saved(&self) -> Saved {
        from_items(self.level, &self.items)
    }
}

fn scan_budgeted(cwd: &str) -> Evidence {
    scan(cwd, 40, std::time::Duration::from_secs(8))
}

pub fn view(ui: &Ui, width: usize) -> View {
    let head = vec![Line::from(
        Level::ALL
            .iter()
            .flat_map(|l| [theme::chip(l.name(), *l == ui.level, *l == ui.level, theme::bold()), Span::raw(" ")])
            .collect::<Vec<_>>(),
    )];
    if ui.loading {
        return View::new("Optimize", vec![Line::styled("  reading this project's history and Claude Code's skill list…", theme::dim())], 0, theme::hints(&[("esc", "cancel")])).with_head(head);
    }
    let sel = ui.sel.min(ui.items.len().saturating_sub(1));
    let mut body: Vec<Line> = vec![];
    let mut group = "";
    let mut focus = 0;
    for (n, it) in ui.items.iter().enumerate() {
        if it.group != group {
            group = it.group;
            body.push(Line::styled(format!("  {group}"), theme::dim()));
        }
        if n == sel {
            focus = body.len();
        }
        let size = if it.saves > 0 { format!("−{}", crate::usage::tokens(it.saves)) } else { String::new() };
        let label: String = it.label.chars().take(width.saturating_sub(24)).collect();
        let pad = width.saturating_sub(12).saturating_sub(label.chars().count()).saturating_sub(size.chars().count()).max(1);
        let style = if n == sel { theme::bold() } else if it.on { ratatui::style::Style::new() } else { theme::dim() };
        body.push(row_with(n == sel, false, vec![Span::styled(if it.on { "☑ " } else { "☐ " }, if it.on { theme::ok() } else { theme::dim() }), Span::styled(label, style), Span::raw(" ".repeat(pad)), Span::styled(size, theme::dim())]));
    }
    let ev = ui.ev.clone().unwrap_or_default();
    let mut tail = vec![Line::raw("")];
    let of = if ui.start_tokens > 0 { format!(" of {} at session start", crate::usage::tokens(ui.start_tokens)) } else { " at session start".into() };
    tail.push(Line::from(vec![Span::styled(format!("≈ −{}", crate::usage::tokens(ui.saves())), theme::bold()), Span::styled(of, theme::dim())]));
    let mut note = ui.level.blurb().to_string();
    note += &format!(". Evidence: {} past sessions, {} tools used{}.", ev.sessions, ev.tools.len(), if ev.partial { " (history scan cut short)" } else { "" });
    if ev.sessions < MIN_SESSIONS {
        note += " Too little history here to judge unused skills, so only the description cap is proposed.";
    }
    tail.extend(wrap(&note, width.saturating_sub(4).max(20)).into_iter().map(|l| Line::styled(l, theme::dim())));
    if let Some(it) = ui.items.get(sel) {
        tail.extend(wrap(&it.why, width.saturating_sub(4).max(20)).into_iter().map(|l| Line::styled(l, theme::dim())));
    }
    View::new("Optimize", body, focus, theme::hints(&[("↑↓", "move"), ("space", "tick"), ("←→", "level"), ("enter", "apply"), ("esc", "cancel")])).with_head(head).with_tail(tail).rows(0, 14)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(sessions: usize, skills: &[&str], tools: &[&str], vocab: &[&str]) -> Evidence {
        Evidence {
            sessions,
            tools: tools.iter().map(|t| (t.to_string(), 1)).collect(),
            skills: skills.iter().map(|t| (t.to_string(), 1)).collect(),
            vocab: vocab.iter().map(|s| s.to_string()).collect(),
            partial: false,
        }
    }

    fn doc() -> instructions::Source {
        let comfy = "# Comfy workflows\nGenerate images, videos and audio through ComfyUI checkpoints, samplers, templates, fragments and blueprints.\n".to_string() + &"Workflow templates feed checkpoint loaders and sampler nodes for diffusion rendering.\n".repeat(60);
        let rust = "# Rust notes\nCargo workspace layout, clippy warnings, terminal rendering with ratatui.\n".to_string() + &"Cargo clippy ratatui terminal rendering workspace layout.\n".repeat(60);
        let text = format!("{comfy}{rust}");
        instructions::Source { path: "/x/AGENTS.md".into(), secs: instructions::sections(&text), text }
    }

    #[test]
    fn levels_escalate_and_only_unused_things_are_touched() {
        let skills = vec![("comfy".to_string(), 300), ("review".to_string(), 200), ("tiny".to_string(), 8)];
        let files = vec![doc()];
        let e = ev(10, &["review"], &["Bash", "Read", "Grep"], &["cargo", "clippy", "ratatui", "terminal", "rendering", "workspace", "layout", "warnings"]);
        let at = |level| propose(&Input { level, ev: &e, skills: &skills, files: &files });
        let light = at(Level::Light);
        assert!(light.len() == 1 && matches!(light[0].change, Change::Cap(160)), "light only caps descriptions");
        let bal = at(Level::Balanced);
        assert!(bal.iter().any(|i| i.change == Change::Skill("comfy".into(), "name-only")), "unused big skill listed by name");
        assert!(!bal.iter().any(|i| matches!(&i.change, Change::Skill(n, _) if n == "review" || n == "tiny")), "used and tiny skills untouched");
        assert!(bal.iter().any(|i| matches!(&i.change, Change::Section(_, id) if id.starts_with(BRIEF) && id.contains("Comfy"))), "unrelated section shortened");
        assert!(!bal.iter().any(|i| matches!(&i.change, Change::Section(_, id) if id.contains("Rust"))), "related section kept");
        let agg = at(Level::Aggressive);
        assert!(agg.iter().any(|i| i.change == Change::Skill("comfy".into(), "off")));
        assert!(agg.iter().any(|i| matches!(&i.change, Change::Tools(t) if t.contains(&"Grep".to_string()) && t.contains(&"Bash".to_string()))));
        assert!(agg.iter().any(|i| i.change == Change::StrictMcp));
        assert!(agg.iter().map(|i| i.saves).sum::<u64>() > bal.iter().map(|i| i.saves).sum::<u64>());
    }

    #[test]
    #[ignore = "reads this machine's history and instruction files; prints the proposals"]
    fn real_project_proposals() {
        let cwd = std::env::current_dir().unwrap();
        let cwd = cwd.to_str().unwrap();
        let e = scan(cwd, 40, std::time::Duration::from_secs(8));
        eprintln!("sessions {} tools {:?} skills {:?} vocab {}", e.sessions, e.tools, e.skills, e.vocab.len());
        let files = instructions::Ui::new(cwd, Rules::new()).files;
        for level in [Level::Balanced, Level::Aggressive] {
            for it in propose(&Input { level, ev: &e, skills: &[], files: &files }).iter().filter(|i| i.group != "skills") {
                eprintln!("{} | {} | −{} | {}", level.name(), it.label, it.saves, it.why);
            }
        }
    }

    #[test]
    fn with_little_history_nothing_is_called_unused() {
        let skills = vec![("comfy".to_string(), 300)];
        let e = ev(1, &[], &[], &[]);
        let p = propose(&Input { level: Level::Aggressive, ev: &e, skills: &skills, files: &[] });
        assert!(p.iter().all(|i| matches!(i.change, Change::Cap(_))), "{p:?}");
    }

    #[test]
    fn saved_round_trips_applies_once_and_clears() {
        let d = std::env::temp_dir().join(format!("claudeplusplus-opt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let cwd = d.to_str().unwrap();
        let items = vec![
            Item { group: "skills", label: String::new(), why: String::new(), saves: 1, on: true, change: Change::Cap(120) },
            Item { group: "skills", label: String::new(), why: String::new(), saves: 1, on: true, change: Change::Skill("a".into(), "off") },
            Item { group: "skills", label: String::new(), why: String::new(), saves: 1, on: false, change: Change::Skill("b".into(), "off") },
            Item { group: "tools", label: String::new(), why: String::new(), saves: 0, on: true, change: Change::Tools(vec!["Bash".into(), "Read".into()]) },
            Item { group: "tools", label: String::new(), why: String::new(), saves: 0, on: true, change: Change::StrictMcp },
        ];
        let saved = from_items(Level::Balanced, &items);
        assert!(!saved.skills.contains_key("b"), "unticked items are not saved");
        save(cwd, &saved).unwrap();
        assert_eq!(load(cwd), Some(saved.clone()));
        let (mut settings, mut args) = (Map::new(), vec![]);
        saved.apply(&mut settings, &mut args);
        assert_eq!(settings["skillListingMaxDescChars"], 120);
        assert_eq!(settings["skillOverrides"]["a"], "off");
        assert_eq!(args, ["--tools", "Bash,Read", "--strict-mcp-config"]);
        let mut chosen = vec!["--tools".to_string(), "Bash".to_string()];
        saved.apply(&mut Map::new(), &mut chosen);
        assert_eq!(chosen.iter().filter(|a| *a == "--tools").count(), 1, "the profile's own tool list wins");
        assert!(clear(cwd) && load(cwd).is_none() && !clear(cwd));
    }

    #[test]
    fn scan_counts_tools_skills_and_slash_commands_from_transcripts() {
        let d = std::env::temp_dir().join(format!("claudeplusplus-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let proj = d.join("proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::write(proj.join("src/pipeline_widget.rs"), "").unwrap();
        let home = d.join("cfg/projects").join(proj.to_str().unwrap().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>());
        std::fs::create_dir_all(&home).unwrap();
        let lines = [
            r#"{"type":"user","message":{"role":"user","content":"please refactor the telemetry exporter"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"review"}},{"type":"tool_use","name":"Grep","input":{}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/loop</command-name> x"}}"#,
        ];
        std::fs::write(home.join("a.jsonl"), lines.join("\n")).unwrap();
        // SAFETY: only this test sets CLAUDE_CONFIG_DIR
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", d.join("cfg")) };
        let e = scan(proj.to_str().unwrap(), 5, std::time::Duration::from_secs(5));
        assert_eq!(e.sessions, 1);
        assert_eq!((e.tools["Skill"], e.tools["Grep"], e.skills["review"], e.skills["loop"]), (1, 1, 1, 1));
        assert!(e.vocab.contains("telemetry") && e.vocab.contains("pipeline") && e.vocab.contains("widget"));
    }
}
