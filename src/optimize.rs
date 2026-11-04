//! `/optimize`: trims what a new session loads, for one project, from evidence instead of guesses. It reads
//! what this project's past sessions actually used (tools, skills) and how much each instruction section
//! has to do with the project, proposes changes at a chosen level, and saves the ticked ones to
//! `<project>/.claude++/optimize.json`. Nothing it saves edits a file of yours, and `/optimize off` deletes it.
//!
//! What a change does, all through Claude Code's own settings and flags (no model call):
//! - skills: `skillListingMaxDescChars` caps every description; `skillOverrides` lists a skill by name only
//!   (`name-only`). Never `off`: a listed name lets the model load the skill with the Skill tool when it
//!   turns out to matter, for about 12 tokens, where a hidden skill could never be found.
//! - instructions: a section is cut to its heading and first line, or replaced by a one-line pointer to its
//!   lines; either way the model is told where the text is and can read it; see `instructions.rs`.
//! - tools (aggressive only): `--tools` is narrowed to the core set plus what the project used, and MCP
//!   servers are left out when none were ever called.
//!
//! Which skills and sections count as unrelated is decided by evidence and word overlap, with no model call,
//! unless you press `c` in the panel: then a small model ([`REVIEW_MODEL`], one call with no tools, MCP,
//! settings or saved session) is shown the project and every skill and large section, and its verdicts
//! replace the word-overlap guesses. Skills the project has used are kept whatever it says.
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
            Level::Aggressive => "also: strongly unrelated sections replaced by a pointer, tools narrowed to what the project used",
        }
    }
    fn cap(self) -> usize {
        match self { Level::Light => 160, Level::Balanced => 120, Level::Aggressive => 60 }
    }
}

/// Skills and tools below/above these are never touched or always kept.
const CORE_TOOLS: [&str; 10] = ["Bash", "Read", "Edit", "Write", "Glob", "Grep", "Agent", "AskUserQuestion", "WebFetch", "WebSearch"];
/// Kept by every tool list `/optimize` narrows to: without `Skill` every skill disappears (name-only ones too),
/// and without `ToolSearch` Claude Code stops deferring MCP tools and sends their full schemas (measured on
/// 2.1.312: 3.9k tokens loaded vs 0.6k loaded + 3.3k deferred). Which skills show is the skill items' business.
const KEEP_REACHABLE: [&str; 2] = ["Skill", "ToolSearch"];
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
    /// Keep only these servers (names as you defined them); leave the rest out.
    McpKeep(Vec<String>),
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
    /// Names of the MCP servers you defined for this folder (see `mcp::defs`).
    pub mcp: &'a [String],
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
            Level::Balanced | Level::Aggressive => Some(("name-only", tokens.saturating_sub(NAME_ONLY_TOKENS))),
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
        let why = format!("never used in {} past sessions here; {}", i.ev.sessions, if mode == "off" { "hidden from the model" } else { "listed by name: the model can still load it, and /name works" });
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
                let saves = (sec.bytes / BYTES_PER_TOKEN) as u64 - if hide { 30 } else { 60 }; // what the pointer or brief line costs
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
        let mut tools: BTreeSet<String> = CORE_TOOLS.iter().chain(&KEEP_REACHABLE).map(|s| s.to_string()).collect();
        tools.extend(i.ev.tools.keys().filter(|t| t.chars().next().is_some_and(char::is_uppercase) && !t.starts_with("mcp__")).cloned());
        out.push(Item { group: "tools", label: format!("limit built-in tools to {}", tools.iter().cloned().collect::<Vec<_>>().join(", ")), why: "the core set plus every tool this project used".into(), saves: 0, on: true, change: Change::Tools(tools.into_iter().collect()) });
        let called = crate::mcp::used(i.ev.tools.keys().map(String::as_str));
        if called.is_empty() {
            out.push(Item { group: "tools", label: "leave MCP servers out".into(), why: "no MCP tool was ever called here".into(), saves: 0, on: true, change: Change::StrictMcp });
        } else {
            // Keep the servers that were called; leave the others out. Only possible when every called server is one
            // you defined (a plugin's or a connector's cannot go in a config file, so then nothing changes).
            let keep: Vec<String> = i.mcp.iter().filter(|n| called.contains(&crate::mcp::norm(n))).cloned().collect();
            let kept: BTreeSet<_> = keep.iter().map(|n| crate::mcp::norm(n)).collect();
            let out_of: Vec<&String> = i.mcp.iter().filter(|n| !kept.contains(&crate::mcp::norm(n))).collect();
            if called.is_subset(&kept) && !out_of.is_empty() {
                let left = out_of.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
                out.push(Item { group: "tools", label: format!("keep only the MCP servers used here ({})", keep.join(", ")), why: format!("never called here: {left}"), saves: 0, on: true, change: Change::McpKeep(keep) });
            }
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
    pub mcp_keep: Option<Vec<String>>,
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
            Change::McpKeep(k) => s.mcp_keep = Some(k.clone()),
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
        "mcp_keep": s.mcp_keep,
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
        mcp_keep: v["mcp_keep"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()),
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
    pub fn apply(&self, settings: &mut Map<String, Value>, args: &mut Vec<String>, cwd: &str) {
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
        // A profile that already chose its MCP servers keeps them. If the kept servers are gone from your
        // config, nothing is left out: losing every server would be worse than keeping them all.
        if let Some(keep) = &self.mcp_keep && !args.iter().any(|a| a == "--strict-mcp-config" || a == "--mcp-config") && let Some(f) = crate::mcp::subset_file(cwd, keep) {
            args.extend(["--strict-mcp-config".into(), "--mcp-config".into(), f.to_string_lossy().into_owned()]);
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
    /// A review by Claude in flight.
    pub review: Option<Pending>,
    /// What Claude was asked about, and its answers (once in, they shape every level's proposal).
    pub cands: Vec<Cand>,
    pub verdicts: Option<Vec<Verdict>>,
    /// (message, is an error) about the review.
    pub review_note: Option<(String, bool)>,
    /// The MCP servers you defined for this folder.
    pub mcp: Vec<String>,
}

pub type Pending = Arc<Mutex<Option<Result<(Vec<Verdict>, f64), String>>>>;

impl Ui {
    pub fn open(cwd: &str, level: Level) -> Ui {
        let scan: Shared = Arc::default();
        let (out, dir) = (scan.clone(), cwd.to_string());
        std::thread::spawn(move || {
            let ev = scan_budgeted(&dir);
            *out.lock().unwrap() = Some(ev);
        });
        let files = instructions::Ui::new(cwd, Rules::new()).files;
        Ui { level, sel: 0, items: vec![], loading: true, ev: None, scan, skills: None, files, start_tokens: 0, review: None, cands: vec![], verdicts: None, review_note: None, mcp: crate::mcp::defs(cwd).into_keys().collect() }
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
        self.items = propose(&Input { level: self.level, ev, skills, files: &self.files, mcp: &self.mcp });
        if let Some(v) = &self.verdicts {
            self.items = with_verdicts(std::mem::take(&mut self.items), self.level, ev, &self.cands, v);
        }
        self.sel = 0;
    }

    /// Ask Claude about every skill and large section, in the background (`bin`/`env`: the claude to run).
    pub fn ask_claude(&mut self, bin: &str, env: &[(String, String)], cwd: &str) {
        let (Some(ev), Some(skills)) = (&self.ev, &self.skills) else { return };
        if self.review.is_some() {
            return;
        }
        if self.level == Level::Light {
            self.review_note = Some(("light changes no skill or section; pick balanced or aggressive, then c".into(), true));
            return;
        }
        self.cands = candidates(skills, &self.files);
        let prompt = review_prompt(cwd, ev, &self.cands);
        let pending: Pending = Arc::default();
        let (out, bin, env) = (pending.clone(), bin.to_string(), env.to_vec());
        std::thread::spawn(move || {
            let r = ask(&bin, &env, &prompt);
            *out.lock().unwrap() = Some(r);
        });
        self.review = Some(pending);
        self.review_note = None;
    }

    /// Take Claude's answer when it is in; true when it changed the proposal.
    pub fn poll_review(&mut self) -> bool {
        let Some(r) = self.review.as_ref().and_then(|p| p.lock().unwrap().take()) else { return false };
        self.review = None;
        match r {
            Ok((v, cost)) => {
                let changed = v.iter().filter(|x| x.1 != "keep").count();
                self.review_note = Some((format!("Claude ({REVIEW_MODEL}) reviewed {} items, ${cost:.4}: {changed} to trim, the rest kept. Its reasons are under each row", self.cands.len()), false));
                self.verdicts = Some(v);
                self.rebuild();
                true
            }
            Err(e) => {
                self.review_note = Some((format!("Claude's review failed: {e}"), true));
                false
            }
        }
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

// ---- asking Claude ----

/// The model asked to review: cheap, and the question needs judgement about relevance, not depth.
pub const REVIEW_MODEL: &str = "haiku";
/// Sections offered to the model, at most (the prompt stays a few thousand tokens).
const MAX_SECTIONS: usize = 80;

/// Something the model is asked about.
#[derive(Clone, Debug, PartialEq)]
pub enum Cand {
    Skill { name: String, tokens: u64 },
    Section { path: String, id: String, title: String, bytes: usize, from: usize, to: usize, preview: String },
}

/// The model's answer for one candidate: (its index, `keep`/`shorten`/`hide`, why).
pub type Verdict = (usize, String, String);

/// Every listed skill worth trimming, and every instruction section big enough to matter.
pub fn candidates(skills: &[(String, u64)], files: &[instructions::Source]) -> Vec<Cand> {
    let mut out: Vec<Cand> = skills.iter().filter(|(_, t)| *t >= 15).map(|(n, t)| Cand::Skill { name: n.clone(), tokens: *t }).collect();
    let secs = files.iter().flat_map(|src| src.secs.iter().filter(|s| s.bytes >= 1500).map(move |s| (src, s)));
    for (src, sec) in secs.take(MAX_SECTIONS) {
        let body: String = src.text.lines().skip(sec.from + 1).take(sec.to.saturating_sub(sec.from + 1)).collect::<Vec<_>>().join(" ");
        let preview = body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
        out.push(Cand::Section { path: src.path.clone(), id: sec.id.clone(), title: sec.title.clone(), bytes: sec.bytes, from: sec.from, to: sec.to, preview });
    }
    out
}

fn review_prompt(cwd: &str, ev: &Evidence, cands: &[Cand]) -> String {
    let mut p = format!("Project folder: {}\n", Path::new(cwd).file_name().map_or(cwd.into(), |n| n.to_string_lossy()));
    let mut top: Vec<String> = std::fs::read_dir(cwd).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect();
    top.sort();
    top.truncate(60);
    p += &format!("Top-level entries: {}\n", top.join(", "));
    if let Some(r) = ["README.md", "README"].iter().find_map(|f| std::fs::read_to_string(Path::new(cwd).join(f)).ok()) {
        p += &format!("README (start):\n{}\n", r.chars().take(1500).collect::<String>());
    }
    let counts = |m: &BTreeMap<String, usize>| m.iter().map(|(k, n)| format!("{k}×{n}")).collect::<Vec<_>>().join(", ");
    p += &format!("Tools used in the last {} sessions here: {}\nSkills and commands used: {}\n\n", ev.sessions, counts(&ev.tools), counts(&ev.skills));
    p += "Each candidate below is loaded into every new Claude Code session in this project. For each id decide:\n\
          - keep: likely useful for work in this project\n\
          - shorten: skills are listed by name only (still loadable); sections are cut to their heading and first line (the full text stays readable on demand)\n\
          - hide: sections only, replaced by a one-line pointer to the text\n\
          When unsure, keep. Give one verdict per id.\n\n";
    for (i, c) in cands.iter().enumerate() {
        p += &match c {
            Cand::Skill { name, tokens } => format!("c{i}: skill `{name}` (~{tokens} tokens)\n"),
            Cand::Section { path, title, bytes, preview, .. } => {
                let file = Path::new(path).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
                format!("c{i}: section `{title}` of {file} (~{} tokens): {preview}\n", bytes / BYTES_PER_TOKEN)
            }
        };
    }
    p
}

/// One call to `bin` with no tools, MCP, settings or saved session; the prompt goes on stdin.
fn ask(bin: &str, env: &[(String, String)], prompt: &str) -> Result<(Vec<Verdict>, f64), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    const SYSTEM: &str = "You review what a coding project loads into every session, to cut what is unrelated. Answer only with the requested JSON. Reasons are one short sentence.";
    const SCHEMA: &str = r#"{"type":"object","properties":{"verdicts":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"action":{"type":"string","enum":["keep","shorten","hide"]},"reason":{"type":"string"}},"required":["id","action","reason"]}}},"required":["verdicts"]}"#;
    let mut child = Command::new(bin)
        .args(["-p", "--model", REVIEW_MODEL, "--output-format", "json", "--tools", "", "--strict-mcp-config", "--setting-sources", "", "--disable-slash-commands", "--no-session-persistence", "--system-prompt", SYSTEM, "--json-schema", SCHEMA])
        .envs(env.iter().cloned())
        .current_dir(std::env::temp_dir()) // no project CLAUDE.md: the prompt carries what it needs
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {bin}: {e}"))?;
    child.stdin.take().map(|mut i| i.write_all(prompt.as_bytes())).transpose().map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    parse_answer(&out.stdout).map_err(|e| {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.trim().is_empty() { e } else { format!("{e}: {}", err.trim().lines().last().unwrap_or_default()) }
    })
}

fn parse_answer(stdout: &[u8]) -> Result<(Vec<Verdict>, f64), String> {
    let v: Value = serde_json::from_slice(stdout).map_err(|_| "no answer".to_string())?;
    if v["is_error"] == true {
        return Err(v["result"].as_str().unwrap_or("error").to_string());
    }
    let list = v["structured_output"]["verdicts"].as_array().ok_or("the answer had no verdicts")?;
    let verdicts = list
        .iter()
        .filter_map(|x| {
            let i = x["id"].as_str()?.trim_start_matches('c').parse().ok()?;
            Some((i, x["action"].as_str()?.to_string(), x["reason"].as_str().unwrap_or_default().to_string()))
        })
        .collect();
    Ok((verdicts, v["total_cost_usd"].as_f64().unwrap_or(0.0)))
}

/// `items` with its skill and section rows replaced by the model's verdicts, within what `level` allows:
/// light changes neither, and only aggressive hides a section (balanced shortens it instead).
pub fn with_verdicts(mut items: Vec<Item>, level: Level, ev: &Evidence, cands: &[Cand], verdicts: &[Verdict]) -> Vec<Item> {
    if level == Level::Light {
        return items;
    }
    items.retain(|i| !matches!(i.change, Change::Skill(..) | Change::Section(..)));
    let used = |name: &str| ev.skills.contains_key(name) || ev.skills.keys().any(|k| k.ends_with(&format!(":{name}")));
    let mut sorted: Vec<&Verdict> = verdicts.iter().filter(|v| v.1 != "keep").collect();
    sorted.sort_by_key(|v| v.0); // parents before their subsections
    let (mut skills, mut secs, mut taken) = (vec![], vec![], vec![]);
    for (i, action, reason) in sorted {
        let why = format!("Claude ({REVIEW_MODEL}): {reason}");
        match cands.get(*i) {
            Some(Cand::Skill { name, tokens }) if !used(name) => {
                skills.push(Item { group: "skills", label: format!("{name}  → name-only"), why, saves: tokens.saturating_sub(NAME_ONLY_TOKENS), on: true, change: Change::Skill(name.clone(), "name-only") });
            }
            Some(Cand::Section { path, id, title, bytes, from, to, .. }) => {
                if taken.iter().any(|(p, a, b)| p == path && from >= a && to <= b) {
                    continue; // inside a section already trimmed
                }
                taken.push((path.clone(), *from, *to));
                let hide = action == "hide" && level == Level::Aggressive;
                let name = Path::new(path).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
                secs.push(Item {
                    group: "instructions",
                    label: format!("{} {title}  ({name})", if hide { "hide" } else { "shorten" }),
                    why,
                    saves: (bytes / BYTES_PER_TOKEN) as u64 - if hide { 30 } else { 60 },
                    on: true,
                    change: Change::Section(path.clone(), if hide { id.clone() } else { format!("{BRIEF}{id}") }),
                });
            }
            _ => {}
        }
    }
    skills.sort_by_key(|s| std::cmp::Reverse(s.saves));
    let rest = items.split_off(items.iter().position(|i| i.group != "skills").unwrap_or(items.len()));
    items.extend(skills);
    items.extend(secs);
    items.extend(rest);
    items
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
    let review = match (&ui.review, &ui.review_note) {
        (Some(_), _) => Some((format!("asking Claude ({REVIEW_MODEL}) about {} skills and sections…", ui.cands.len()), theme::accent())),
        (None, Some((n, err))) => Some((n.clone(), if *err { theme::danger() } else { theme::ok() })),
        (None, None) if ui.verdicts.is_none() => Some((format!("c asks Claude ({REVIEW_MODEL}, about a cent) to judge every skill and section instead of word overlap"), theme::dim())),
        _ => None,
    };
    if let Some((n, style)) = review {
        tail.extend(wrap(&n, width.saturating_sub(4).max(20)).into_iter().map(|l| Line::styled(l, style)));
    }
    View::new("Optimize", body, focus, theme::hints(&[("↑↓", "move"), ("space", "tick"), ("←→", "level"), ("c", "ask Claude"), ("enter", "apply"), ("esc", "cancel")])).with_head(head).with_tail(tail).rows(0, 14)
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
        instructions::Source { path: "/x/CLAUDE.md".into(), secs: instructions::sections(&text), text }
    }

    #[test]
    fn claudes_verdicts_replace_the_guesses_within_the_level() {
        let e = ev(5, &["used"], &["Bash"], &["cargo", "clippy", "ratatui"]);
        let skills = vec![("used".to_string(), 80), ("pptx".to_string(), 90), ("tiny".to_string(), 5)];
        let files = [doc()];
        let cands = candidates(&skills, &files);
        assert_eq!(cands.iter().filter(|c| matches!(c, Cand::Skill { .. })).count(), 2, "a skill too small to matter is not asked about");
        let at = |name: &str| cands.iter().position(|c| match c { Cand::Skill { name: n, .. } => n == name, Cand::Section { title, .. } => title == name }).unwrap();
        let prompt = review_prompt("/nonexistent", &e, &cands);
        assert!(prompt.contains(&format!("c{}: skill `pptx`", at("pptx"))) && prompt.contains("section `Rust notes`"));
        let v: Vec<Verdict> = vec![
            (at("used"), "shorten".into(), "x".into()),
            (at("pptx"), "shorten".into(), "slides, not Rust".into()),
            (at("Comfy workflows"), "hide".into(), "image generation".into()),
            (at("Rust notes"), "keep".into(), "this is Rust".into()),
            (999, "hide".into(), "a made-up id".into()),
        ];
        let base = propose(&Input { level: Level::Balanced, ev: &e, skills: &skills, files: &files, mcp: &[] });
        let out = with_verdicts(base, Level::Balanced, &e, &cands, &v);
        let labels: Vec<&str> = out.iter().map(|i| i.label.as_str()).collect();
        assert!(labels[0].starts_with("cap skill"), "the cap stays first: {labels:?}");
        assert!(labels.contains(&"pptx  → name-only"));
        assert!(!labels.iter().any(|l| l.starts_with("used")), "a skill the project used is kept whatever Claude says");
        assert!(labels.iter().any(|l| l.starts_with("shorten Comfy workflows")), "balanced shortens what Claude would hide: {labels:?}");
        assert!(!labels.iter().any(|l| l.contains("Rust notes")), "keep means no row, even if word overlap had proposed one");
        assert!(out.iter().find(|i| i.label.starts_with("pptx")).unwrap().why.contains("slides, not Rust"));
        let aggressive = with_verdicts(propose(&Input { level: Level::Aggressive, ev: &e, skills: &skills, files: &files, mcp: &[] }), Level::Aggressive, &e, &cands, &v);
        assert!(aggressive.iter().any(|i| i.label.starts_with("hide Comfy workflows")));
        assert!(aggressive.last().unwrap().group == "tools", "tool rows stay at the end");
        let light = propose(&Input { level: Level::Light, ev: &e, skills: &skills, files: &files, mcp: &[] });
        assert_eq!(with_verdicts(light.clone(), Level::Light, &e, &cands, &v).len(), light.len(), "light changes no skill or section");
    }

    #[test]
    fn the_answer_is_read_from_structured_output() {
        let ok = br#"{"is_error":false,"total_cost_usd":0.0012,"structured_output":{"verdicts":[{"id":"c3","action":"hide","reason":"r"},{"id":"bad","action":"keep","reason":""}]}}"#;
        assert_eq!(parse_answer(ok), Ok((vec![(3, "hide".to_string(), "r".to_string())], 0.0012)));
        assert_eq!(parse_answer(br#"{"is_error":true,"result":"Not logged in"}"#), Err("Not logged in".to_string()));
        assert!(parse_answer(b"garbage").is_err());
        assert!(parse_answer(br#"{"is_error":false}"#).is_err());
    }

    #[test]
    fn levels_escalate_and_only_unused_things_are_touched() {
        let skills = vec![("comfy".to_string(), 300), ("review".to_string(), 200), ("tiny".to_string(), 8)];
        let files = vec![doc()];
        let e = ev(10, &["review"], &["Bash", "Read", "Grep"], &["cargo", "clippy", "ratatui", "terminal", "rendering", "workspace", "layout", "warnings"]);
        let at = |level| propose(&Input { level, ev: &e, skills: &skills, files: &files, mcp: &[] });
        let light = at(Level::Light);
        assert!(light.len() == 1 && matches!(light[0].change, Change::Cap(160)), "light only caps descriptions");
        let bal = at(Level::Balanced);
        assert!(bal.iter().any(|i| i.change == Change::Skill("comfy".into(), "name-only")), "unused big skill listed by name");
        assert!(!bal.iter().any(|i| matches!(&i.change, Change::Skill(n, _) if n == "review" || n == "tiny")), "used and tiny skills untouched");
        assert!(bal.iter().any(|i| matches!(&i.change, Change::Section(_, id) if id.starts_with(BRIEF) && id.contains("Comfy"))), "unrelated section shortened");
        assert!(!bal.iter().any(|i| matches!(&i.change, Change::Section(_, id) if id.contains("Rust"))), "related section kept");
        let agg = at(Level::Aggressive);
        assert!(agg.iter().any(|i| i.change == Change::Skill("comfy".into(), "name-only")), "never hidden outright: the model must still be able to find it");
        let tools = agg.iter().find_map(|i| if let Change::Tools(t) = &i.change { Some(t.clone()) } else { None }).expect("aggressive narrows tools");
        assert!(tools.iter().any(|t| t == "Skill") && tools.iter().any(|t| t == "ToolSearch"), "skills and deferred tools stay reachable: {tools:?}");
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
            for it in propose(&Input { level, ev: &e, skills: &[], files: &files, mcp: &[] }).iter().filter(|i| i.group != "skills") {
                eprintln!("{} | {} | −{} | {}", level.name(), it.label, it.saves, it.why);
            }
        }
    }

    #[test]
    fn with_little_history_nothing_is_called_unused() {
        let skills = vec![("comfy".to_string(), 300)];
        let e = ev(1, &[], &[], &[]);
        let p = propose(&Input { level: Level::Aggressive, ev: &e, skills: &skills, files: &[], mcp: &[] });
        assert!(p.iter().all(|i| matches!(i.change, Change::Cap(_))), "{p:?}");
    }

    #[test]
    fn mcp_servers_the_project_called_are_kept_and_the_rest_left_out() {
        let mcp: Vec<String> = ["caveman", "codegraph", "comfy-mcp"].map(String::from).to_vec();
        let at = |tools: &[&str], mcp: &[String]| propose(&Input { level: Level::Aggressive, ev: &ev(5, &[], tools, &[]), skills: &[], files: &[], mcp });
        let keep = |p: &[Item]| p.iter().find_map(|i| if let Change::McpKeep(k) = &i.change { Some(k.clone()) } else { None });
        let p = at(&["Bash", "mcp__codegraph__codegraph_explore", "mcp__comfy-mcp__run"], &mcp);
        assert_eq!(keep(&p), Some(vec!["codegraph".to_string(), "comfy-mcp".to_string()]));
        assert!(p.iter().any(|i| i.why.contains("never called here: caveman")), "{p:?}");
        assert!(!p.iter().any(|i| i.change == Change::StrictMcp), "not all of them go");
        // A called server we cannot put in a config file (a plugin's) means everything stays.
        let p = at(&["mcp__plugin_openviking-memory_openviking__search"], &mcp);
        assert!(keep(&p).is_none() && !p.iter().any(|i| i.change == Change::StrictMcp), "{p:?}");
        // Every defined server was called: nothing to leave out. None called: all of them go, as before.
        assert!(keep(&at(&["mcp__caveman__a", "mcp__codegraph__b", "mcp__comfy-mcp__c"], &mcp)).is_none());
        assert!(at(&["Bash"], &mcp).iter().any(|i| i.change == Change::StrictMcp));
        // It is saved and read back.
        let saved = from_items(Level::Aggressive, &at(&["mcp__codegraph__x"], &mcp));
        assert_eq!(saved.mcp_keep, Some(vec!["codegraph".to_string()]));
        let d = std::env::temp_dir().join(format!("claudeplusplus-optkeep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        save(d.to_str().unwrap(), &saved).unwrap();
        assert_eq!(load(d.to_str().unwrap()), Some(saved));
        let _ = std::fs::remove_dir_all(&d);
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
        saved.apply(&mut settings, &mut args, cwd);
        assert_eq!(settings["skillListingMaxDescChars"], 120);
        assert_eq!(settings["skillOverrides"]["a"], "off");
        assert_eq!(args, ["--tools", "Bash,Read", "--strict-mcp-config"]);
        let mut chosen = vec!["--tools".to_string(), "Bash".to_string()];
        saved.apply(&mut Map::new(), &mut chosen, cwd);
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
