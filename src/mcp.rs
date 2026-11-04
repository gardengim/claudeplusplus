//! MCP servers by name: which ones a folder defines, which ones its past sessions called, and a config
//! file holding just the ones to keep. `/optimize` (aggressive) uses this to leave out the servers a project never
//! touches while keeping the ones it does: a server is a child process (measured 13 to 92 MB each) and its tools
//! add prompt tokens.
//!
//! Only servers you defined yourself can be picked one by one: `~/.claude.json` (user scope, then this
//! folder's), and `<folder>/.mcp.json`. Servers from plugins and claude.ai connectors cannot be listed in a
//! config file, so a project that called one of them keeps everything.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Claude Code's spelling of a server's name inside tool names (`mcp__<name>__<tool>`).
pub fn norm(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn claude_json() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    Some(dir.join(".claude.json"))
}

fn servers_of(v: &Value) -> impl Iterator<Item = (String, Value)> + '_ {
    v["mcpServers"].as_object().into_iter().flatten().map(|(k, d)| (k.clone(), d.clone()))
}

/// The servers you defined that a session in `cwd` would load, by name.
pub fn defs(cwd: &str) -> BTreeMap<String, Value> {
    defs_in(claude_json().as_deref(), cwd)
}

fn defs_in(claude_json: Option<&Path>, cwd: &str) -> BTreeMap<String, Value> {
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let mut out = BTreeMap::new();
    if let Some(v) = claude_json.and_then(read) {
        out.extend(servers_of(&v));
        out.extend(servers_of(&v["projects"][cwd]));
    }
    if let Some(v) = read(&Path::new(cwd).join(".mcp.json")) {
        out.extend(servers_of(&v));
    }
    out
}

/// The servers (as tool names spell them) that these tool names belong to.
pub fn used<'a>(tools: impl Iterator<Item = &'a str>) -> BTreeSet<String> {
    tools.filter_map(|t| t.strip_prefix("mcp__")?.split("__").next().filter(|s| !s.is_empty()).map(String::from)).collect()
}

/// A config file with just `keep` (names from [`defs`]), for `--mcp-config`. None when none of them is defined
/// any more or the file cannot be written: the caller then changes nothing.
pub fn subset_file(cwd: &str, keep: &[String]) -> Option<PathBuf> {
    write_subset(&defs(cwd), &crate::config::cache_dir()?, keep)
}

fn write_subset(all: &BTreeMap<String, Value>, dir: &Path, keep: &[String]) -> Option<PathBuf> {
    let chosen: BTreeMap<_, _> = keep.iter().filter_map(|k| Some((k.clone(), all.get(k)?.clone()))).collect();
    if chosen.is_empty() {
        return None;
    }
    let text = serde_json::to_string_pretty(&json!({ "mcpServers": chosen })).ok()?;
    let h = text.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let path = dir.join(format!("mcp-{h:016x}.json"));
    if std::fs::read_to_string(&path).ok().as_deref() != Some(&text) {
        crate::config::write_atomic(&path, &text).ok()?;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_spelled_as_tool_names_do_and_usage_is_read_from_them() {
        assert_eq!(norm("claude.ai Claude Docs"), "claude_ai_Claude_Docs");
        assert_eq!(norm("comfy-mcp"), "comfy-mcp");
        let u = used(["mcp__codegraph__codegraph_explore", "Bash", "mcp__comfy-mcp__run", "mcp__codegraph__x", "mcp__"].into_iter());
        assert_eq!(u.into_iter().collect::<Vec<_>>(), ["codegraph", "comfy-mcp"]);
    }

    #[test]
    fn servers_come_from_the_user_the_folder_and_its_mcp_json_and_a_subset_file_holds_only_the_asked_ones() {
        let d = std::env::temp_dir().join(format!("claudeplusplus-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let proj = d.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_str().unwrap();
        let cj = d.join("claude.json");
        std::fs::write(&cj, serde_json::to_string(&json!({
            "mcpServers": {"user-one": {"command": "a"}},
            "projects": {cwd: {"mcpServers": {"local-one": {"command": "b"}}}, "/elsewhere": {"mcpServers": {"not-mine": {"command": "c"}}}}
        })).unwrap()).unwrap();
        std::fs::write(proj.join(".mcp.json"), r#"{"mcpServers": {"shared": {"command": "d"}}}"#).unwrap();
        let all = defs_in(Some(&cj), cwd);
        assert_eq!(all.keys().collect::<Vec<_>>(), ["local-one", "shared", "user-one"], "another folder's servers are not ours");
        assert!(defs_in(None, "/nowhere").is_empty());
        // The file: only what was asked, and a name that is gone is skipped.
        let cache = d.join("cache");
        let f = write_subset(&all, &cache, &["shared".into(), "gone".into()]).expect("a file");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(v["mcpServers"].as_object().unwrap().keys().collect::<Vec<_>>(), ["shared"]);
        assert_eq!(write_subset(&all, &cache, &["shared".into()]), Some(f), "same servers, same file");
        assert!(write_subset(&all, &cache, &["gone".into()]).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
