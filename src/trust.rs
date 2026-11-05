//! Folder trust. Interactive Claude Code asks "do you trust this folder?" before it runs anything the folder
//! brings: MCP servers from `.mcp.json`, hooks, helper commands in project settings. `claude -p`, which is what
//! claude++ drives, never asks (measured: an unapproved `.mcp.json` server is started and connected). So claude++
//! asks in its place, once per folder: a folder Claude Code has recorded as trusted, or one that brings nothing
//! that runs, starts at once; any other waits for `/trust`.

use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Folders `/trust` allowed in this run.
static ALLOWED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

pub fn allow(cwd: &Path) {
    ALLOWED.lock().unwrap_or_else(|e| e.into_inner()).insert(cwd.to_path_buf());
}

fn allowed(cwd: &Path) -> bool {
    let a = ALLOWED.lock().unwrap_or_else(|e| e.into_inner());
    cwd.ancestors().any(|d| a.contains(d))
}

/// Claude Code's own record of "you trusted this folder", from `~/.claude.json`, checked for `cwd` and
/// every parent as Claude Code does. Project settings may run commands, so they wait for this.
pub fn trusted(cwd: &Path, claude_json: Option<&Path>) -> bool {
    let Some(v) = claude_json.and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return false;
    };
    cwd.ancestors().any(|a| v["projects"][a.to_string_lossy().as_ref()]["hasTrustDialogAccepted"] == true)
}

/// Project settings that make Claude Code run something of the folder's.
const RUNS: [&str; 6] = ["hooks", "apiKeyHelper", "awsAuthRefresh", "awsCredentialExport", "otelHeadersHelper", "env"];

/// What the folder (and its parents, up to its git root; never the home folder itself) brings that would run.
pub fn runs(cwd: &Path, home: Option<&Path>) -> Vec<String> {
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let mut out = vec![];
    for dir in cwd.ancestors() {
        if Some(dir) == home || dir.parent().is_none() {
            break;
        }
        if let Some(v) = read(&dir.join(".mcp.json")) {
            let names: Vec<&str> = v["mcpServers"].as_object().into_iter().flatten().map(|(k, _)| k.as_str()).collect();
            if !names.is_empty() {
                out.push(format!("{} starts MCP servers ({})", dir.join(".mcp.json").display(), names.join(", ")));
            }
        }
        for file in [".claude/settings.json", ".claude/settings.local.json"] {
            let path = dir.join(file);
            let keys: Vec<&str> = read(&path).map(|v| RUNS.iter().copied().filter(|k| v.get(*k).is_some_and(|x| !x.is_null())).collect()).unwrap_or_default();
            if !keys.is_empty() {
                out.push(format!("{} sets {}", path.display(), keys.join(", ")));
            }
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    out
}

/// May claude start in `cwd`? `Err` is what to tell the user.
pub fn check(cwd: &Path) -> Result<(), String> {
    check_in(cwd, crate::mcp::claude_json().as_deref(), std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

fn check_in(cwd: &Path, claude_json: Option<&Path>, home: Option<&Path>) -> Result<(), String> {
    if allowed(cwd) || trusted(cwd, claude_json) {
        return Ok(());
    }
    let list = runs(cwd, home);
    if list.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Not started: Claude Code has not been told to trust {}, and claude++ runs `claude -p`, which does not ask before running a folder's own commands. This folder has: {}. Look at them, then open the folder once in `claude` and accept its trust dialog, or type /trust here to start anyway (this run only).",
        cwd.display(),
        list.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("claudeplusplus-trust-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".claude")).unwrap();
        d
    }

    #[test]
    fn a_folder_that_brings_something_to_run_waits_until_trusted_or_allowed() {
        let d = dir("gate");
        let home = d.join("home");
        let proj = d.join("proj/sub");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(proj.join(".claude")).unwrap();
        std::fs::create_dir_all(d.join("proj/.git")).unwrap();
        // Nothing that runs: starts at once.
        assert!(check_in(&proj, None, Some(&home)).is_ok());
        std::fs::write(proj.join(".claude/settings.json"), r#"{"permissions": {"allow": ["Bash(ls)"]}, "model": "opus"}"#).unwrap();
        assert!(check_in(&proj, None, Some(&home)).is_ok(), "settings that only configure are not commands");
        // A project server and a hook: waits, and says what.
        std::fs::write(d.join("proj/.mcp.json"), r#"{"mcpServers": {"evil": {"command": "sh"}}}"#).unwrap();
        std::fs::write(proj.join(".claude/settings.local.json"), r#"{"hooks": {"SessionStart": []}, "env": {"A": "1"}}"#).unwrap();
        let why = check_in(&proj, None, Some(&home)).unwrap_err();
        assert!(why.contains("starts MCP servers (evil)") && why.contains("sets hooks, env") && why.contains("/trust"), "{why}");
        // Claude Code's own record of trust, for the folder or a parent, lets it through.
        let cj = d.join("claude.json");
        std::fs::write(&cj, serde_json::json!({"projects": {d.join("proj").to_string_lossy(): {"hasTrustDialogAccepted": true}}}).to_string()).unwrap();
        assert!(check_in(&proj, Some(&cj), Some(&home)).is_ok());
        std::fs::write(&cj, serde_json::json!({"projects": {d.join("proj").to_string_lossy(): {"hasTrustDialogAccepted": false}}}).to_string()).unwrap();
        assert!(check_in(&proj, Some(&cj), Some(&home)).is_err(), "a recorded refusal is not trust");
        // `/trust` allows the folder (and what is under it) for this run.
        allow(&proj);
        assert!(check_in(&proj, Some(&cj), Some(&home)).is_ok());
        assert!(check_in(&proj.join("deeper"), Some(&cj), Some(&home)).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_home_folder_and_what_is_above_a_git_root_are_not_searched() {
        let d = dir("home");
        std::fs::write(d.join(".mcp.json"), r#"{"mcpServers": {"x": {"command": "sh"}}}"#).unwrap();
        // `d` plays the home folder: its own files are the user's, not a project's.
        assert!(runs(&d, Some(&d)).is_empty());
        let repo = d.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(runs(&repo, None).is_empty(), "a git root ends the search");
        let _ = std::fs::remove_dir_all(&d);
    }
}
