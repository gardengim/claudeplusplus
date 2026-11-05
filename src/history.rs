//! Reads Claude Code's on-disk session transcripts (`~/.claude/projects/<cwd>/<id>.jsonl`) so a
//! resumed tab can show earlier messages and `/resume` can list sessions. The format is internal
//! to Claude Code, so parsing is lenient: anything not recognised is skipped.

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Only the tail of a long transcript is replayed, to keep memory flat.
const MAX_LINES: usize = 400;

pub struct Summary {
    pub id: String,
    /// `/rename` title, else Claude Code's generated one, else the first prompt.
    pub title: String,
    /// First real prompt, kept for filtering.
    pub prompt: String,
    pub modified: SystemTime,
    pub size: u64,
    /// The directory the conversation ran in (empty if the transcript does not say).
    pub cwd: String,
}

fn projects_dir() -> Option<PathBuf> {
    Some(crate::config::claude_dir()?.join("projects"))
}

/// Claude Code names a project's folder after its path, every non-alphanumeric char becoming `-`.
/// Claude Code's project folder name: every UTF-16 unit that isn't ASCII alphanumeric becomes '-';
/// past 200 units it cuts there and appends a base-36 Java-style hash of the path.
fn encode_cwd(cwd: &str) -> String {
    let name: String = cwd
        .chars()
        .flat_map(|c| if c.is_ascii_alphanumeric() { vec![c] } else { vec!['-'; c.len_utf16()] })
        .collect();
    if name.len() <= 200 {
        return name;
    }
    let h = cwd.encode_utf16().fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(u as i32));
    let (mut n, mut digits) = ((h as i64).unsigned_abs(), Vec::new());
    loop {
        digits.push(char::from_digit((n % 36) as u32, 36).unwrap());
        n /= 36;
        if n == 0 {
            break;
        }
    }
    format!("{}-{}", &name[..200], digits.iter().rev().collect::<String>())
}

/// Locate a session's transcript by id, whichever project folder holds it.
pub fn find(id: &str) -> Option<PathBuf> {
    // Ids are UUIDs; refuse anything else so a typed argument cannot point outside the projects dir.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    std::fs::read_dir(projects_dir()?)
        .ok()?
        .flatten()
        .map(|d| d.path().join(format!("{id}.jsonl")))
        .find(|p| p.is_file())
}

/// Delete a conversation: its transcript and the folder of subagent transcripts beside it.
pub fn delete(id: &str) -> std::io::Result<()> {
    let path = find(id).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "transcript not found"))?;
    std::fs::remove_file(&path)?;
    let _ = std::fs::remove_dir_all(path.with_extension(""));
    Ok(())
}

/// Transcript files of a working directory's sessions, newest first.
pub fn transcripts(cwd: &str, n: usize) -> Vec<PathBuf> {
    let Some(dir) = projects_dir().map(|d| d.join(encode_cwd(cwd))) else { return vec![] };
    let mut files: Vec<(PathBuf, SystemTime)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files.into_iter().take(n).map(|f| f.0).collect()
}

/// Most recent sessions for a working directory, newest first.
pub fn recent(cwd: &str, n: usize) -> Vec<Summary> {
    projects_dir().map(|d| recent_in(&[d.join(encode_cwd(cwd))], n)).unwrap_or_default()
}

/// Most recent sessions of every project, newest first: a conversation you closed in another folder
/// is still one you want back.
pub fn recent_all(n: usize) -> Vec<Summary> {
    let Some(root) = projects_dir() else { return vec![] };
    let dirs: Vec<PathBuf> = std::fs::read_dir(root).into_iter().flatten().flatten().map(|d| d.path()).filter(|p| p.is_dir()).collect();
    recent_in(&dirs, n)
}

fn recent_in(dirs: &[PathBuf], n: usize) -> Vec<Summary> {
    let mut files: Vec<(PathBuf, SystemTime)> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    files.truncate(n);
    files
        .into_iter()
        .map(|(p, modified)| {
            let size = std::fs::metadata(&p).map_or(0, |m| m.len());
            let (title, prompt, cwd) = cached_titles(&p, modified, size);
            Summary { id: p.file_stem().unwrap_or_default().to_string_lossy().into_owned(), title, prompt, modified, size, cwd }
        })
        .collect()
}

type Titles = (String, String, String);
type Cached = (SystemTime, u64, Titles);

/// Listing re-reads often (the side panel refreshes); a transcript is only re-read when it changed.
fn cached_titles(path: &Path, modified: SystemTime, size: u64) -> Titles {
    static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);
    if let Ok(c) = CACHE.lock()
        && let Some((m, sz, t)) = c.as_ref().and_then(|c| c.get(path))
        && *m == modified
        && *sz == size
    {
        return t.clone();
    }
    let t = titles(path);
    if let Ok(mut c) = CACHE.lock() {
        c.get_or_insert_with(HashMap::new).insert(path.to_path_buf(), (modified, size, t.clone()));
    }
    t
}

/// How much of a transcript's end is scanned for titles and previews, to keep listing cheap.
const TAIL_BYTES: u64 = 256 * 1024;

fn squash(t: &str, n: usize) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(n).collect()
}

/// (display title, first prompt, directory). Titles are appended as the session goes, so the tail is scanned too.
fn titles(path: &Path) -> Titles {
    let n = names(path);
    let title = [n.custom, n.ai].into_iter().find(|t| !t.trim().is_empty()).map_or_else(|| squash(&n.prompt, 60), |t| squash(&t, 60));
    (title, n.prompt, n.cwd)
}

/// The name a session shows elsewhere: the `/rename` or `--name` title, else the generated one.
pub fn session_name(path: &Path) -> Option<String> {
    let n = names(path);
    [n.custom, n.ai].into_iter().find(|t| !t.trim().is_empty())
}

/// The conversation's `/color`, if it has one (`default` counts as none).
pub fn session_color(path: &Path) -> Option<String> {
    Some(names(path).color).filter(|c| !c.is_empty() && c != "default")
}

/// What a transcript says about itself, each empty if absent.
#[derive(Default)]
struct Names {
    custom: String,
    ai: String,
    prompt: String,
    cwd: String,
    color: String,
}

fn names(path: &Path) -> Names {
    let Ok(mut f) = std::fs::File::open(path) else { return Default::default() };
    let mut n = Names::default();
    let mut title_line = |line: &str| {
        let (key, out) = if line.contains("\"type\":\"custom-title\"") {
            ("customTitle", &mut n.custom)
        } else if line.contains("\"type\":\"ai-title\"") {
            ("aiTitle", &mut n.ai)
        } else if line.contains("\"type\":\"agent-color\"") {
            ("agentColor", &mut n.color)
        } else {
            return;
        };
        if let Some(t) = serde_json::from_str::<Value>(line).ok().and_then(|v| v[key].as_str().map(String::from)) {
            *out = t;
        }
    };
    for line in BufReader::new(&f).lines().take(300).map_while(Result::ok) {
        title_line(&line);
    }
    let len = f.metadata().map_or(0, |m| m.len());
    let from = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(from)).is_ok() {
        // A mid-file seek lands inside a line; drop that partial one.
        for line in BufReader::new(&f).lines().skip((from > 0) as usize).map_while(Result::ok) {
            title_line(&line);
        }
    }
    if let Ok(f) = std::fs::File::open(path) {
        let users = BufReader::new(f).lines().take(300).map_while(Result::ok).filter(|l| l.contains("\"type\":\"user\"")).filter_map(|l| serde_json::from_str::<Value>(&l).ok());
        for v in users {
            if n.cwd.is_empty() {
                n.cwd = v["cwd"].as_str().unwrap_or_default().to_string();
            }
            if n.prompt.is_empty() {
                n.prompt = user_text(&v).map(|t| squash(&t, 200)).unwrap_or_default();
            }
            if !n.cwd.is_empty() && !n.prompt.is_empty() {
                break;
            }
        }
    }
    n
}

/// The last few exchanges of a transcript as (is_user, text), read from its tail only.
pub fn preview(path: &Path, n: usize) -> Vec<(bool, String)> {
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let from = f.metadata().map_or(0, |m| m.len()).saturating_sub(TAIL_BYTES * 2);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return vec![];
    }
    let mut out: VecDeque<(bool, String)> = VecDeque::new();
    for line in BufReader::new(f).lines().skip((from > 0) as usize).map_while(Result::ok) {
        if !(line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let entry = if let Some(t) = user_text(&v) {
            Some((true, t))
        } else if v["type"] == "assistant" && v["isSidechain"] != true {
            let t = v["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (!t.trim().is_empty()).then_some((false, t))
        } else {
            None
        };
        if let Some((u, t)) = entry {
            if out.len() == n {
                out.pop_front();
            }
            out.push_back((u, t.chars().take(600).collect()));
        }
    }
    out.into()
}

/// The text of a real user prompt, or None for tool results, injected context, and bookkeeping.
pub fn user_text(v: &Value) -> Option<String> {
    if v["type"] != "user" || v["isSidechain"] == true || v["isMeta"] == true || v["isCompactSummary"] == true {
        return None;
    }
    let text = match &v["message"]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) if a.iter().all(|b| b["type"] != "tool_result") => a
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
                Some("image") => "[image]".into(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let mut t = text.trim();
    // A message sent after `!command`s carries their output first; the words typed come after it.
    if t.starts_with("<bash-input>") {
        t = t.rfind("</bash-").and_then(|i| t[i..].find('>').map(|j| t[i + j + 1..].trim())).unwrap_or("");
    }
    let noise = ["<command-", "<local-command", "<system-reminder", "Caveat:", "[Request interrupted", "<bash-", "<task-notification>"];
    (!t.is_empty() && !noise.iter().any(|n| t.starts_with(n))).then(|| t.to_string())
}

/// The entries of the conversation as it now stands, which leaves out turns a rewind abandoned:
/// walked back from the newest message by `parentUuid` (across compaction by `logicalParentUuid`),
/// plus side branches that start without a typed prompt (parallel tool calls fork the chain too).
/// None for a transcript without links (then every line counts).
fn chain(path: &Path) -> Option<std::collections::HashSet<String>> {
    // Substring reads, not a parse: transcripts reach hundreds of MB. The key appears once per line,
    // the top-level `uuid` after the message (so the last match is it).
    fn value<'a>(line: &'a str, at: Option<usize>, key: &str) -> Option<&'a str> {
        let rest = &line[at? + key.len()..];
        rest.find('"').map(|end| &rest[..end])
    }
    let f = std::fs::File::open(path).ok()?;
    // uuid -> (parent, is a typed prompt)
    let mut info: HashMap<String, (Option<String>, bool)> = HashMap::new();
    let mut order = Vec::new();
    let mut leaf = None;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        const UUID: &str = "\"uuid\":\"";
        let Some(u) = value(&line, line.rfind(UUID), UUID) else { continue };
        let up = ["\"parentUuid\":\"", "\"logicalParentUuid\":\""].iter().find_map(|k| value(&line, line.find(k), k));
        let user = line.starts_with("{\"type\":\"user\"") || line.contains(",\"type\":\"user\"");
        let message = (user || line.contains(",\"type\":\"assistant\"")) && !line.contains("\"isSidechain\":true");
        // Bookkeeping lines may hang off anything; the conversation ends at its newest message.
        if message {
            leaf = Some(u.to_string());
        }
        info.insert(u.to_string(), (up.map(String::from), user && !line.contains("\"tool_use_id\"")));
        order.push(u.to_string());
    }
    let mut keep: HashMap<String, bool> = HashMap::new();
    let mut at = leaf;
    while let Some(u) = at {
        if keep.insert(u.clone(), true).is_some() {
            break; // a loop: stop rather than spin
        }
        at = info.get(&u).and_then(|i| i.0.clone());
    }
    if keep.is_empty() {
        return None;
    }
    for u in order {
        let mut path = Vec::new();
        let mut cur = Some(u);
        let verdict = loop {
            let Some(c) = cur else { break false };
            if let Some(&v) = keep.get(&c) {
                break v;
            }
            let Some((up, prompt)) = info.get(&c) else { break false };
            if path.len() > info.len() {
                break false;
            }
            let (up, prompt) = (up.clone(), *prompt);
            path.push(c);
            if prompt {
                break false;
            }
            cur = up;
        };
        keep.extend(path.into_iter().map(|c| (c, verdict)));
    }
    Some(keep.into_iter().filter_map(|(u, v)| v.then_some(u)).collect())
}

/// Reports of a conversation's subagents, by the id of the Agent call that started each: the message of
/// the agent's last `SubagentHandback`, read from `<transcript>/subagents/agent-*.jsonl` (the `.meta.json`
/// beside each names its call).
pub fn agent_reports(transcript: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(dir) = std::fs::read_dir(transcript.with_extension("").join("subagents")) else { return out };
    for meta in dir.flatten().map(|d| d.path()).filter(|p| p.to_string_lossy().ends_with(".meta.json")) {
        let Some(call) = std::fs::read_to_string(&meta).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|m| m["toolUseId"].as_str().map(String::from)) else {
            continue;
        };
        let log = PathBuf::from(meta.to_string_lossy().replace(".meta.json", ".jsonl"));
        let Ok(f) = std::fs::File::open(&log) else { continue };
        let report = BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter(|l| l.contains("\"SubagentHandback\"") && l.contains("\"tool_use\""))
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .flat_map(|v| v["message"]["content"].as_array().cloned().unwrap_or_default())
            .filter(|b| b["type"] == "tool_use" && b["name"] == "SubagentHandback")
            .filter_map(|b| b["input"]["message"].as_str().map(String::from))
            .last();
        if let Some(r) = report {
            out.insert(call, r);
        }
    }
    out
}

/// The typed prompts of the conversation as it now stands (its current branch), oldest first. Streams the
/// file and parses only user lines, so a rewind on a huge transcript stays quick.
pub fn prompts(path: &Path) -> Vec<Value> {
    let on = chain(path);
    let Ok(f) = std::fs::File::open(path) else { return vec![] };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter(|l| l.contains("\"type\":\"user\"") && !l.contains("\"tool_use_id\""))
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| user_text(v).is_some() && on.as_ref().is_none_or(|on| v["uuid"].as_str().is_none_or(|u| on.contains(u))))
        .collect()
}

/// The last `MAX_LINES` top-level user/assistant lines, plus how many earlier ones were left out.
pub fn tail(path: &Path) -> (Vec<Value>, usize) {
    let on = chain(path);
    let Ok(f) = std::fs::File::open(path) else { return (vec![], 0) };
    let mut keep: VecDeque<Value> = VecDeque::new();
    let mut dropped = 0;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        // Cheap prefilter: the transcript is mostly API request bookkeeping we never parse.
        if !(line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if !matches!(v["type"].as_str(), Some("user" | "assistant")) || v["isSidechain"] == true {
            continue;
        }
        if let (Some(on), Some(u)) = (&on, v["uuid"].as_str())
            && !on.contains(u)
        {
            continue;
        }
        if keep.len() == MAX_LINES {
            keep.pop_front();
            dropped += 1;
        }
        keep.push_back(v);
    }
    (keep.into(), dropped)
}

pub fn size(b: u64) -> String {
    match b {
        0..=1023 => format!("{b}B"),
        1024..=1048575 => format!("{}K", b / 1024),
        _ => format!("{:.1}M", b as f64 / 1048576.0),
    }
}

pub fn ago(t: SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_resume_shows_the_branch_a_rewind_left_with_parallel_tools_and_compaction_intact() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-chain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let line = |kind: &str, uuid: &str, parent: &str, content: &str| {
            let parent = if parent.is_empty() { "null".to_string() } else { format!("\"{parent}\"") };
            format!(r#"{{"parentUuid":{parent},"isSidechain":false,"type":"{kind}","message":{{"role":"{kind}","content":{content}}},"uuid":"{uuid}"}}"#)
        };
        let tr = |id: &str| format!(r#"[{{"type":"tool_result","tool_use_id":"{id}","content":"ok"}}]"#);
        let tu = |id: &str| format!(r#"[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{}}}}]"#);
        let text = |t: &str| format!(r#"[{{"type":"text","text":"{t}"}}]"#);
        let lines = [
            r#"{"type":"attachment","attachment":{"type":"hook_success"},"parentUuid":null,"uuid":"h0"}"#.to_string(),
            line("user", "p1", "h0", r#""first""#),
            // Two parallel tool calls: the second result hangs off the first call, beside the second.
            line("assistant", "a1", "p1", &tu("t1")),
            line("assistant", "a2", "a1", &tu("t2")),
            line("user", "r2", "a2", &tr("t2")),
            line("user", "r1", "a1", &tr("t1")),
            line("assistant", "a3", "r1", &text("both ran")),
            // Compaction restarts the chain; the old one stays reachable.
            r#"{"parentUuid":null,"logicalParentUuid":"a3","type":"system","subtype":"compact_boundary","uuid":"c0"}"#.to_string(),
            line("user", "p2", "c0", r#""abandoned""#),
            line("assistant", "a4", "p2", &text("gone")),
            // Rewound to before "abandoned" and asked again.
            line("user", "p3", "c0", r#""second""#),
            line("assistant", "a5", "p3", &text("kept")),
        ];
        std::fs::write(&p, lines.join("\n")).unwrap();
        let (kept, _) = tail(&p);
        let ids: Vec<&str> = kept.iter().filter_map(|v| v["uuid"].as_str()).collect();
        assert_eq!(ids, ["p1", "a1", "a2", "r2", "r1", "a3", "p3", "a5"]);
        let typed: Vec<String> = prompts(&p).iter().filter_map(|v| v["uuid"].as_str().map(String::from)).collect();
        assert_eq!(typed, ["p1", "p3"], "a rewind matches only prompts on the branch shown");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subagents_report_is_found_by_the_call_that_started_it() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-agents-{}", std::process::id()));
        let sub = dir.join("s").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("agent-x.meta.json"), r#"{"toolUseId":"toolu_1","finished":true}"#).unwrap();
        let handback = |m: &str| format!(r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"h","name":"SubagentHandback","input":{{"message":"{m}"}}}}]}}}}"#);
        std::fs::write(sub.join("agent-x.jsonl"), [handback("draft"), handback("final report")].join("\n")).unwrap();
        std::fs::write(sub.join("agent-y.meta.json"), r#"{"toolUseId":"toolu_2"}"#).unwrap(); // no log: skipped
        let r = agent_reports(&dir.join("s.jsonl"));
        assert_eq!((r.get("toolu_1").map(String::as_str), r.len()), (Some("final report"), 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_names_match_claude_codes_encoding() {
        // Observed on disk for this very project.
        assert_eq!(encode_cwd("/Users/me/Projects/claude++"), "-Users-me-Projects-claude--");
        // Expected values computed by Claude Code's own function (2.1.311) under node.
        assert_eq!(encode_cwd("/a/😀b"), "-a---b");
        let long = format!("/Users/me/{}😀x", "deeply-nested-folder/".repeat(10));
        let want = format!("-Users-me-{}d-71wv3g", "deeply-nested-folder-".repeat(9));
        assert_eq!(encode_cwd(&long), want);
    }

    #[test]
    fn user_text_keeps_prompts_and_drops_noise() {
        let u = |c: Value| json!({"type": "user", "message": {"content": c}});
        assert_eq!(user_text(&u(json!("fix it"))).as_deref(), Some("fix it"));
        assert_eq!(user_text(&u(json!([{"type": "text", "text": "see"}, {"type": "image"}]))).as_deref(), Some("see [image]"));
        assert_eq!(user_text(&u(json!([{"type": "tool_result", "content": "x"}]))), None);
        assert_eq!(user_text(&u(json!("<command-name>/model</command-name>"))), None);
        assert_eq!(user_text(&json!({"type": "user", "isMeta": true, "message": {"content": "hi"}})), None);
        assert_eq!(user_text(&json!({"type": "assistant", "message": {"content": "hi"}})), None);
    }

    #[test]
    fn find_refuses_path_tricks_and_recent_orders_by_mtime() {
        assert!(find("../../etc/passwd").is_none() && find("").is_none());
        let dir = std::env::temp_dir().join(format!("claudeplusplus-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let line = |t: &str| format!("{{\"type\":\"queue-operation\"}}\n{{\"type\":\"user\",\"message\":{{\"content\":\"{t}\"}}}}\n");
        std::fs::write(dir.join("aaa.jsonl"), line("older prompt")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(dir.join("bbb.jsonl"), line("newer prompt")).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let r = recent_in(std::slice::from_ref(&dir), 5);
        assert_eq!(r.iter().map(|s| (s.id.as_str(), s.title.as_str())).collect::<Vec<_>>(), vec![("bbb", "newer prompt"), ("aaa", "older prompt")]);
    }

    #[test]
    fn titles_prefer_rename_then_generated_then_first_prompt() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-titles-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let user = "{\"type\":\"user\",\"cwd\":\"/work/app\",\"message\":{\"content\":\"first prompt here\"}}\n";
        let ai = "{\"type\":\"ai-title\",\"aiTitle\":\"Generated\"}\n";
        let custom = "{\"type\":\"custom-title\",\"customTitle\":\"Renamed\"}\n";
        let t = |name: &str, body: String| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            titles(&p)
        };
        assert_eq!(t("a", user.into()), ("first prompt here".into(), "first prompt here".into(), "/work/app".into()));
        assert_eq!(t("b", format!("{user}{ai}")).0, "Generated");
        assert_eq!(t("c", format!("{user}{ai}{custom}")).0, "Renamed");
        // A title appended after a long body is found through the tail scan.
        let filler = format!("{{\"type\":\"x\",\"pad\":\"{}\"}}\n", "y".repeat(2000));
        assert_eq!(t("d", format!("{user}{}{custom}", filler.repeat(400))).0, "Renamed");
    }

    #[test]
    fn the_last_color_wins_and_default_is_none() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-color-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let line = |c: &str| format!("{{\"type\":\"agent-color\",\"agentColor\":\"{c}\"}}\n");
        let color = |name: &str, body: String| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            session_color(&p)
        };
        assert_eq!(color("a", String::new()), None);
        assert_eq!(color("b", line("red") + &line("blue")).as_deref(), Some("blue"));
        assert_eq!(color("c", line("red") + &line("default")), None);
    }

    #[test]
    fn preview_returns_last_turns_with_roles() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-prev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("p.jsonl");
        std::fs::write(&p, concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"q1\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\"},{\"type\":\"text\",\"text\":\"a1\"}]}}\n",
            "{\"type\":\"user\",\"message\":{\"content\":\"q2\"}}\n",
        )).unwrap();
        assert_eq!(preview(&p, 2), vec![(false, "a1".to_string()), (true, "q2".to_string())]);
    }

    #[test]
    fn tail_keeps_last_lines_and_skips_sidechains() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = String::from("{\"type\":\"api-request\"}\n{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"content\":\"sub\"}}\n");
        for i in 0..(MAX_LINES + 5) {
            s += &format!("{{\"type\":\"user\",\"message\":{{\"content\":\"m{i}\"}}}}\n");
        }
        let p = dir.join("t.jsonl");
        std::fs::write(&p, s).unwrap();
        let (lines, dropped) = tail(&p);
        assert_eq!((lines.len(), dropped), (MAX_LINES, 5));
        assert_eq!(lines.last().unwrap()["message"]["content"], format!("m{}", MAX_LINES + 4));
    }
}


/// Prompts sent from `cwd`, oldest first, from Claude Code's own `history.jsonl` (shared with
/// interactive `claude`, so ↑ recalls what was typed in either). Only the file's tail is read.
pub fn sent_prompts(cwd: &str) -> Vec<String> {
    let Some(path) = crate::config::claude_dir().map(|d| d.join("history.jsonl")) else { return vec![] };
    prompts_in(&path, cwd)
}

fn prompts_in(path: &Path, cwd: &str) -> Vec<String> {
    const TAIL: u64 = 1 << 20;
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL)));
    let mut out: Vec<String> = vec![];
    // Bytes, then lossy: the cut can land inside a character, and one bad line must not end the read.
    for line in BufReader::new(f).split(b'\n').map_while(Result::ok) {
        let line = String::from_utf8_lossy(&line);
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue }; // also skips the cut first line
        if v["project"].as_str() != Some(cwd) {
            continue;
        }
        let Some(mut text) = v["display"].as_str().map(String::from) else { continue };
        // A long paste is stored as a placeholder plus its content (or only a hash, then it cannot come back).
        for (id, p) in v["pastedContents"].as_object().into_iter().flatten() {
            let (Some(kind), content) = (p["type"].as_str(), p["content"].as_str()) else { continue };
            let label = format!("[Pasted text #{id}");
            // `#1` must not match `#10`: the number ends at a space or the bracket.
            let start = text.match_indices(&label).map(|m| m.0).find(|&i| matches!(text[i + label.len()..].chars().next(), Some(' ' | ']')));
            let Some(start) = start.filter(|_| kind == "text") else { continue };
            let Some(content) = content else { text.clear(); break };
            let end = text[start..].find(']').map_or(text.len(), |e| start + e + 1);
            text.replace_range(start..end, content);
        }
        if !text.trim().is_empty() && out.last() != Some(&text) {
            out.push(text);
        }
    }
    out
}

/// Records a sent prompt the way interactive `claude` does, so it shows up in its ↑ history too.
pub fn record_prompt(cwd: &str, text: &str, session: Option<&str>) {
    use std::io::Write;
    if cfg!(test) {
        return; // tests submit messages; they must not land in the real history
    }
    let Some(path) = crate::config::claude_dir().map(|d| d.join("history.jsonl")) else { return };
    let ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let mut v = serde_json::json!({"display": text, "pastedContents": {}, "timestamp": ms, "project": cwd});
    if let Some(s) = session {
        v["sessionId"] = s.into();
    }
    // One write per line with O_APPEND: safe next to a running `claude` appending to the same file.
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(format!("{v}\n").as_bytes());
    }
}

#[cfg(test)]
mod prompt_tests {
    #[test]
    fn prompts_are_this_projects_with_pastes_restored() {
        let d = std::env::temp_dir().join(format!("cpp-hist-{}", std::process::id()));
        let lines = [
            r#"{"display":"one","pastedContents":{},"project":"/p"}"#,
            r#"{"display":"elsewhere","pastedContents":{},"project":"/q"}"#,
            r#"{"display":"see [Pasted text #2 +3 lines] ok","pastedContents":{"2":{"id":2,"type":"text","content":"a\nb\nc"}},"project":"/p"}"#,
            r#"{"display":"[Pasted text #3 +9 lines]","pastedContents":{"3":{"id":3,"type":"text","contentHash":"x"}},"project":"/p"}"#,
            r#"{"display":"one","pastedContents":{},"project":"/p"}"#,
            r#"{"display":"[Pasted text #10 +2 lines] [Pasted text #1 +2 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"one"},"10":{"id":10,"type":"text","content":"ten"}},"project":"/p"}"#,
        ];
        // A cut first line that is not even UTF-8 must not end the read.
        std::fs::write(&d, [&b"\x80\x80\n"[..], lines.join("\n").as_bytes()].concat()).unwrap();
        assert_eq!(super::prompts_in(&d, "/p"), ["one", "see a\nb\nc ok", "one", "ten one"]);
        let _ = std::fs::remove_file(d);
    }
}
