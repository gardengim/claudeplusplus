//! Launch profiles: a name for a set of `claude` flags. Three are built in (`full`, `sources`, `lean`);
//! more are JSON files in `$XDG_CONFIG_HOME/claude++/profiles/<name>.json` (else `~/.config/...`):
//!
//! ```json
//! { "extends": "lean",                       // a built-in or another profile; optional
//!   "append_system_prompt_file": "style.md", // adds to Claude Code's default prompt
//!   "system_prompt": "You are ...",          // or replaces it (no extends needed; omit both to keep the default)
//!   "tools": "Bash,Read,Edit", "setting_sources": "project,local", "strict_mcp": true,
//!   "mcp_config": ["mcp.json"], "model": "haiku", "permission_mode": "plan", "effort": "low",
//!   "add_dirs": ["../shared"], "args": ["--some-flag"], "env": {"KEY": "value"} }
//! ```
//!
//! A child profile's keys replace its parent's (`args` are appended, `env` is merged); relative file paths
//! are relative to the profile file; `~/` is the home directory. Unknown keys are an error, so typos show.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const LEAN_TOOLS: &str = "Bash,Read,Edit,Write,AskUserQuestion,WebFetch,WebSearch";
const KEYS: &[&str] = &[
    "extends", "description", "system_prompt", "system_prompt_file", "append_system_prompt", "append_system_prompt_file", "tools",
    "setting_sources", "strict_mcp", "mcp_config", "model", "permission_mode", "effort", "add_dirs", "args", "env",
];
const MAX_DEPTH: usize = 6;

#[derive(Default, Debug, PartialEq)]
pub struct Spec {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub fn dir() -> Option<PathBuf> {
    Some(crate::config::path()?.with_file_name("profiles"))
}

/// Names of the custom profiles on disk, sorted.
pub fn names() -> Vec<String> {
    let Some(rd) = dir().and_then(|d| std::fs::read_dir(d).ok()) else { return vec![] };
    let mut v: Vec<String> = rd.flatten().filter_map(|e| e.path().file_stem().filter(|_| e.path().extension().is_some_and(|x| x == "json")).map(|s| s.to_string_lossy().into_owned())).collect();
    v.sort();
    v
}

pub fn builtin(name: &str) -> Option<Map<String, Value>> {
    let v = match name {
        "full" => serde_json::json!({}),
        "sources" => serde_json::json!({"setting_sources": "project,local"}),
        "lean" => serde_json::json!({"tools": LEAN_TOOLS, "strict_mcp": true, "setting_sources": "project,local"}),
        _ => return None,
    };
    v.as_object().cloned()
}

pub fn spec(name: &str) -> Result<Spec, String> {
    resolve(dir().as_deref(), name)
}

/// The profile's flags and environment. `dir` is where custom profiles live (`None`: only built-ins).
pub fn resolve(dir: Option<&Path>, name: &str) -> Result<Spec, String> {
    emit(&load(dir, name, &mut vec![])?)
}

/// Merged keys. The nearest profile that sets a path key decides what it is relative to, so each
/// key records its own directory in `__dir:<key>`.
fn load(dir: Option<&Path>, name: &str, seen: &mut Vec<String>) -> Result<Map<String, Value>, String> {
    if let Some(b) = builtin(name) {
        return Ok(b);
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || name.is_empty() {
        return Err(format!("{name:?} is not a profile name (letters, digits, - and _)"));
    }
    if seen.len() >= MAX_DEPTH || seen.iter().any(|s| s == name) {
        return Err(format!("profile {name:?} extends itself ({})", seen.join(" → ")));
    }
    let Some(d) = dir else { return Err(unknown(name)) };
    let file = d.join(format!("{name}.json"));
    let text = std::fs::read_to_string(&file).map_err(|_| unknown(name))?;
    let own: Map<String, Value> = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| if let Value::Object(m) = v { Some(m) } else { None })
        .ok_or_else(|| format!("{} is not a JSON object", file.display()))?;
    if let Some(k) = own.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!("{}: unknown key {k:?} (known: {})", file.display(), KEYS.join(", ")));
    }
    seen.push(name.to_string());
    let mut merged = match own.get("extends").and_then(Value::as_str) {
        Some(parent) => load(dir, parent, seen)?,
        None => Map::new(),
    };
    seen.pop();
    let here = d.to_string_lossy().into_owned();
    // Setting either form of a prompt replaces both forms of the parent's.
    for pair in [["system_prompt", "system_prompt_file"], ["append_system_prompt", "append_system_prompt_file"]] {
        if pair.iter().any(|k| own.contains_key(*k)) {
            pair.iter().for_each(|k| drop(merged.remove(*k)));
        }
    }
    for (k, v) in own {
        match (k.as_str(), merged.get_mut(&k)) {
            ("extends" | "description", _) => {}
            ("args", Some(Value::Array(a))) => a.extend(v.as_array().cloned().unwrap_or_default()),
            ("env", Some(Value::Object(e))) => e.extend(v.as_object().cloned().unwrap_or_default()),
            _ => {
                if matches!(k.as_str(), "system_prompt_file" | "append_system_prompt_file" | "mcp_config" | "add_dirs") {
                    merged.insert(format!("__dir:{k}"), Value::String(here.clone()));
                }
                merged.insert(k, v);
            }
        }
    }
    Ok(merged)
}

fn unknown(name: &str) -> String {
    let custom = names();
    let all = ["full", "sources", "lean"].map(String::from).into_iter().chain(custom).collect::<Vec<_>>();
    format!("unknown profile {name:?} ({})", all.join("|"))
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    }
}

fn path(merged: &Map<String, Value>, key: &str, p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = p.strip_prefix("~/").map(|r| format!("{home}/{r}")).unwrap_or_else(|| p.to_string());
    match merged.get(&format!("__dir:{key}")).and_then(Value::as_str) {
        Some(d) if !Path::new(&p).is_absolute() => Path::new(d).join(p).to_string_lossy().into_owned(),
        _ => p,
    }
}

fn emit(m: &Map<String, Value>) -> Result<Spec, String> {
    let mut a: Vec<String> = vec![];
    let mut flag = |name: &str, vals: Vec<String>| {
        if !vals.is_empty() {
            a.push(name.into());
            a.extend(vals);
        }
    };
    let text = |k: &str| m.get(k).map(strings).unwrap_or_default();
    let paths = |k: &str| text(k).iter().map(|p| path(m, k, p)).collect::<Vec<_>>();
    flag("--system-prompt", text("system_prompt"));
    flag("--system-prompt-file", paths("system_prompt_file"));
    flag("--append-system-prompt", text("append_system_prompt"));
    flag("--append-system-prompt-file", paths("append_system_prompt_file"));
    flag("--tools", vec![text("tools").join(",")].into_iter().filter(|s| !s.is_empty()).collect());
    flag("--setting-sources", vec![text("setting_sources").join(",")].into_iter().filter(|s| !s.is_empty()).collect());
    flag("--model", text("model"));
    flag("--permission-mode", text("permission_mode"));
    flag("--effort", text("effort"));
    for p in paths("mcp_config") {
        flag("--mcp-config", vec![p]);
    }
    for p in paths("add_dirs") {
        flag("--add-dir", vec![p]);
    }
    if m.get("strict_mcp").and_then(Value::as_bool) == Some(true) {
        a.push("--strict-mcp-config".into());
    }
    a.extend(text("args"));
    let env = m.get("env").and_then(Value::as_object).map(|e| e.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()).unwrap_or_default();
    Ok(Spec { args: a, env })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("claudeplusplus-prof-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        files.iter().for_each(|(n, t)| std::fs::write(d.join(format!("{n}.json")), t).unwrap());
        d
    }
    fn args(d: &Path, n: &str) -> Vec<String> {
        resolve(Some(d), n).unwrap().args
    }

    #[test]
    fn built_ins_keep_their_flags_and_custom_ones_derive_from_them() {
        let d = dir("derive", &[
            ("style", r#"{"extends":"lean","append_system_prompt_file":"style.md","tools":["Bash","Read"],"model":"haiku","env":{"A":"1"},"args":["--x"]}"#),
            ("deeper", r#"{"extends":"style","append_system_prompt":"inline","env":{"B":"2"},"args":["--y"],"strict_mcp":false}"#),
        ]);
        assert_eq!(args(&d, "full"), Vec::<String>::new());
        assert_eq!(args(&d, "lean"), ["--tools", LEAN_TOOLS, "--setting-sources", "project,local", "--strict-mcp-config"]);
        let style = args(&d, "style");
        assert_eq!(style[..2], ["--append-system-prompt-file", &format!("{}/style.md", d.display())], "{style:?}");
        assert!(style.windows(2).any(|w| w == ["--tools", "Bash,Read"]) && style.windows(2).any(|w| w == ["--model", "haiku"]));
        assert!(style.contains(&"--strict-mcp-config".to_string()) && style.ends_with(&["--x".to_string()]), "inherits lean's MCP rule: {style:?}");
        let deeper = resolve(Some(&d), "deeper").unwrap();
        assert!(deeper.args.windows(2).any(|w| w == ["--append-system-prompt", "inline"]) && !deeper.args.iter().any(|a| a.ends_with("style.md")), "one append form replaces the other");
        assert!(!deeper.args.contains(&"--strict-mcp-config".to_string()), "child switches it off");
        assert_eq!(deeper.args.iter().filter(|a| a.starts_with("--x") || a.starts_with("--y")).count(), 2, "args accumulate");
        assert_eq!(deeper.env.len(), 2, "env merges");
    }

    #[test]
    fn mistakes_are_errors_not_silent() {
        let d = dir("bad", &[("typo", r#"{"sytem_prompt":"x"}"#), ("loop-a", r#"{"extends":"loop-b"}"#), ("loop-b", r#"{"extends":"loop-a"}"#), ("junk", "[1]")]);
        assert!(resolve(Some(&d), "typo").unwrap_err().contains("unknown key"));
        assert!(resolve(Some(&d), "loop-a").unwrap_err().contains("extends itself"));
        assert!(resolve(Some(&d), "junk").unwrap_err().contains("not a JSON object"));
        assert!(resolve(Some(&d), "nope").unwrap_err().contains("unknown profile"));
        assert!(resolve(Some(&d), "../etc/passwd").unwrap_err().contains("not a profile name"));
        assert!(resolve(None, "full").is_ok());
    }

    #[test]
    fn a_replacing_system_prompt_and_absolute_paths_pass_through() {
        let d = dir("abs", &[("own", r#"{"system_prompt_file":"/abs/p.md","mcp_config":["m.json"],"add_dirs":["/x"]}"#)]);
        let a = args(&d, "own");
        assert_eq!(a[..2], ["--system-prompt-file", "/abs/p.md"]);
        assert!(a.windows(2).any(|w| w[0] == "--mcp-config" && w[1].ends_with("/m.json") && w[1].starts_with(&d.to_string_lossy().to_string())));
        assert!(a.windows(2).any(|w| w == ["--add-dir", "/x"]));
    }
}
