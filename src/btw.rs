//! `/btw <question>`: a side question about this conversation. One `claude -p` call that resumes a copy of the
//! conversation (`--fork-session`, `--no-session-persistence`) with every tool off, so Claude answers from what
//! it has already seen and nothing is written back: the question and the answer never enter the tab's history,
//! its context or its transcript file. The question appears in the tab's transcript (a display-only `Side`
//! entry) and the answer fills in below it when it arrives; no panel opens and no key is taken, so it can be
//! asked while Claude is mid-turn and the turn is not touched. It re-reads the conversation as input, which
//! costs tokens (and may miss the prompt cache, the tool list differing; not measured), so the price is shown.

use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

const SYSTEM: &str = "This is a side question (/btw) asked in passing. Answer it briefly and directly from the conversation so far. You have no tools here, so do not offer to run, read or change anything; if the answer needs that, say so in one line.";

/// The arguments for the call. `session_id` is None before the conversation has started: the question is then
/// asked on its own.
pub fn args(session_id: Option<&str>, model: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--output-format", "json", "--tools", "", "--no-session-persistence", "--append-system-prompt", SYSTEM].map(String::from).into();
    if let Some(id) = session_id {
        a.extend(["--resume", id, "--fork-session"].map(String::from));
    }
    if !model.is_empty() {
        a.extend(["--model".into(), model.into()]);
    }
    a
}

/// Run the call (blocking: call it from a thread). The question goes on stdin, in `cwd` so the conversation is found.
pub fn ask(bin: &str, env: &[(String, String)], cwd: &str, args: &[String], question: &str) -> Result<(String, f64), String> {
    let mut child = Command::new(bin)
        .args(args)
        .envs(env.iter().cloned())
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {bin}: {e}"))?;
    child.stdin.take().map(|mut i| i.write_all(question.as_bytes())).transpose().map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    parse(&out.stdout).map_err(|e| {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.trim().is_empty() { e } else { format!("{e}: {}", err.trim().lines().last().unwrap_or_default()) }
    })
}

pub fn parse(stdout: &[u8]) -> Result<(String, f64), String> {
    let v: Value = serde_json::from_slice(stdout).map_err(|_| "no answer".to_string())?;
    let text = v["result"].as_str().unwrap_or_default().trim().to_string();
    if v["is_error"] == true {
        return Err(if text.is_empty() { "error".into() } else { text });
    }
    if text.is_empty() {
        return Err("the answer was empty".into());
    }
    Ok((text, v["total_cost_usd"].as_f64().unwrap_or(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_started_conversation_is_resumed_as_a_copy_with_no_tools_and_nothing_saved() {
        let a = args(Some("abc"), "opus");
        let has = |f: &str| a.iter().any(|x| x == f);
        assert!(has("--resume") && has("--fork-session") && has("--no-session-persistence") && has("--model"));
        let at = a.iter().position(|x| x == "--tools").unwrap();
        assert_eq!(a[at + 1], "", "an empty tool list");
        let fresh = args(None, "");
        assert!(!fresh.iter().any(|x| x == "--resume" || x == "--fork-session" || x == "--model"));
    }

    #[test]
    fn replies_parse_into_text_and_cost_or_an_error() {
        assert_eq!(parse(br#"{"result":" hi ","total_cost_usd":0.5,"is_error":false}"#).unwrap(), ("hi".into(), 0.5));
        assert_eq!(parse(br#"{"result":"Not logged in","is_error":true}"#).unwrap_err(), "Not logged in");
        assert!(parse(br#"{"result":"","is_error":false}"#).is_err());
        assert!(parse(b"garbage").is_err());
    }
}
