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
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    Some(base.join("projects"))
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
    let (custom, ai, prompt, cwd) = names(path);
    let title = [custom, ai].into_iter().find(|t| !t.trim().is_empty()).map_or_else(|| squash(&prompt, 60), |t| squash(&t, 60));
    (title, prompt, cwd)
}

/// The name a session shows elsewhere: the `/rename` or `--name` title, else the generated one.
pub fn session_name(path: &Path) -> Option<String> {
    let (custom, ai, ..) = names(path);
    [custom, ai].into_iter().find(|t| !t.trim().is_empty())
}

/// (custom title, generated title, first prompt, directory), each empty if absent.
fn names(path: &Path) -> (String, String, String, String) {
    let Ok(mut f) = std::fs::File::open(path) else { return Default::default() };
    let (mut custom, mut ai, mut prompt, mut cwd) = (String::new(), String::new(), String::new(), String::new());
    let mut title_line = |line: &str| {
        let (key, out) = if line.contains("\"type\":\"custom-title\"") {
            ("customTitle", &mut custom)
        } else if line.contains("\"type\":\"ai-title\"") {
            ("aiTitle", &mut ai)
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
            if cwd.is_empty() {
                cwd = v["cwd"].as_str().unwrap_or_default().to_string();
            }
            if prompt.is_empty() {
                prompt = user_text(&v).map(|t| squash(&t, 200)).unwrap_or_default();
            }
            if !cwd.is_empty() && !prompt.is_empty() {
                break;
            }
        }
    }
    (custom, ai, prompt, cwd)
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

/// The last `MAX_LINES` top-level user/assistant lines, plus how many earlier ones were left out.
pub fn tail(path: &Path) -> (Vec<Value>, usize) {
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
    fn folder_names_match_claude_codes_encoding() {
        // Observed on disk for this very project.
        assert_eq!(encode_cwd("/Users/jeongwonkim/Projects/claude++"), "-Users-jeongwonkim-Projects-claude--");
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
