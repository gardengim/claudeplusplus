//! Background sessions Claude Code's supervisor keeps (`~/.claude/jobs/<id>/state.json`): the same
//! states `claude agents` lists, read from small files instead of spawning a process. The format is
//! internal to Claude Code, so parsing is lenient and only what a list row shows is kept.

use serde_json::Value;
use std::path::PathBuf;
use std::time::SystemTime;

/// Sorted by how much it wants the user: blocked first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum State {
    Blocked,
    Working,
    Failed,
    Done,
    Stopped,
}

impl State {
    fn parse(s: &str) -> Option<State> {
        Some(match s {
            "blocked" => State::Blocked,
            "working" => State::Working,
            "failed" => State::Failed,
            "done" => State::Done,
            "stopped" => State::Stopped,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            State::Blocked => "needs you",
            State::Working => "working",
            State::Failed => "failed",
            State::Done => "done",
            State::Stopped => "stopped",
        }
    }
    /// One glyph per state, the same in the session list, the dock and the tab bar.
    pub fn glyph(self) -> &'static str {
        match self {
            State::Blocked => "◆",
            State::Working => "◐",
            State::Failed => "✗",
            State::Done => "✓",
            State::Stopped => "·",
        }
    }
    /// Another process owns the conversation, so a headless `--resume` of it is refused.
    pub fn live(self) -> bool {
        matches!(self, State::Blocked | State::Working)
    }
}

pub struct Job {
    /// Short supervisor id, the argument of `claude attach|stop`.
    pub short: String,
    pub session: String,
    pub state: State,
    pub cwd: Box<str>,
    pub name: Box<str>,
    /// What it is doing or waiting for: the model-written row summary, or the question it is blocked on.
    pub detail: Box<str>,
    pub modified: SystemTime,
}

fn clip(v: &Value, n: usize) -> Box<str> {
    let s = v.as_str().unwrap_or_default();
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(n).collect::<String>().into()
}

fn dir() -> Option<PathBuf> {
    Some(crate::config::claude_dir()?.join("jobs"))
}

/// One `state.json`; `None` when it lacks what a row needs.
fn parse(short: &str, text: &str, modified: SystemTime) -> Option<Job> {
    let v: Value = serde_json::from_str(text).ok()?;
    let state = State::parse(v["state"].as_str()?)?;
    let session = v["sessionId"].as_str().filter(|s| !s.is_empty())?.to_string();
    let needs = clip(&v["needs"], 160);
    Some(Job {
        short: short.into(),
        session,
        state,
        cwd: v["cwd"].as_str().unwrap_or_default().into(),
        name: clip(&v["name"], 60),
        detail: if needs.is_empty() { clip(&v["detail"], 160) } else { needs },
        modified,
    })
}

/// Every supervisor session, newest first.
pub fn load() -> Vec<Job> {
    let Some(rd) = dir().and_then(|d| std::fs::read_dir(d).ok()) else { return vec![] };
    let mut jobs: Vec<Job> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path().join("state.json");
            let modified = std::fs::metadata(&p).ok()?.modified().ok()?;
            parse(&e.file_name().to_string_lossy(), &std::fs::read_to_string(&p).ok()?, modified)
        })
        .collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(j.modified));
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_what_a_row_shows_and_prefers_the_question_a_blocked_job_asks() {
        let t = SystemTime::now();
        let j = parse("ab12cd34", r#"{"state":"blocked","sessionId":"s-1","cwd":"/p","name":"  fix   it ","detail":"did a thing","needs":"pick  A or B","respawnFlags":["--x"],"output":{"result":"big"}}"#, t).unwrap();
        assert_eq!((j.state, &*j.name, &*j.detail, &*j.cwd, j.short.as_str()), (State::Blocked, "fix it", "pick A or B", "/p", "ab12cd34"));
        let j = parse("x", r#"{"state":"done","sessionId":"s-2","detail":"shipped"}"#, t).unwrap();
        assert_eq!((&*j.detail, j.state.live()), ("shipped", false));
        assert!(parse("x", r#"{"state":"weird","sessionId":"s"}"#, t).is_none(), "unknown state");
        assert!(parse("x", r#"{"state":"done"}"#, t).is_none(), "no session id");
        assert!(parse("x", "not json", t).is_none());
        assert!(State::Blocked < State::Working && State::Working < State::Done);
    }
}
