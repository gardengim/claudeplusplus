//! claude++'s own config file, kept apart from Claude Code's settings: `$XDG_CONFIG_HOME/claude++/config.json`
//! (else `~/.config/claude++/config.json`). It is a flat JSON object. Only keys claude++ knows are read, and
//! saving one key keeps every other key as it was, so hand-edits and future options survive.

use serde_json::{Map, Value};
use std::io;
use std::path::{Path, PathBuf};

/// Folder name under the XDG config and cache homes.
pub const DIR: &str = "claude++";

/// `$<var>` if set, else `~/<rel>`: the XDG base folders and Claude Code's own config folder.
fn env_or_home(var: &str, rel: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::home_dir().map(|h| h.join(rel)))
}

/// Claude Code's config folder (`$CLAUDE_CONFIG_DIR`, else `~/.claude`).
pub fn claude_dir() -> Option<PathBuf> {
    env_or_home("CLAUDE_CONFIG_DIR", ".claude")
}

pub fn path() -> Option<PathBuf> {
    Some(env_or_home("XDG_CONFIG_HOME", ".config")?.join(DIR).join("config.json"))
}

fn load(path: &Path) -> Map<String, Value> {
    read(path).unwrap_or_default()
}

/// The file's keys; a missing or empty file is no keys, an unreadable one is an error (so it is never overwritten).
fn read(path: &Path) -> io::Result<Map<String, Value>> {
    let t = match std::fs::read_to_string(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        r => r?,
    };
    if t.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(&t) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a JSON object; fix it by hand first, nothing was changed", path.display()))),
    }
}

/// Replace `path` with `text` through a temp file and a rename, so a crash or a second
/// instance never sees it half-written. Follows a symlink and keeps the file's mode.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = target.clone().into_os_string();
    tmp.push(format!(".claudepp-{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &target).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn get_str(path: &Path, key: &str) -> Option<String> {
    load(path).get(key).and_then(Value::as_str).map(String::from)
}

pub fn set_str(path: &Path, key: &str, value: &str) -> io::Result<()> {
    set_value(path, key, Value::String(value.into()))
}

pub fn get_value(path: &Path, key: &str) -> Option<Value> {
    load(path).remove(key)
}

/// Cache folder (`$XDG_CACHE_HOME/claude++`, else `~/.cache/claude++`).
pub fn cache_dir() -> Option<PathBuf> {
    Some(env_or_home("XDG_CACHE_HOME", ".cache")?.join(DIR))
}

pub fn set_value(path: &Path, key: &str, value: Value) -> io::Result<()> {
    let mut m = read(path)?;
    m.insert(key.into(), value);
    write_atomic(path, &(serde_json::to_string_pretty(&Value::Object(m))? + "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_a_key_keeps_the_others_and_survives_garbage() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("nested").join("config.json");
        assert_eq!(get_str(&file, "statusline"), None);
        set_str(&file, "statusline", "claude").unwrap();
        assert_eq!(get_str(&file, "statusline").as_deref(), Some("claude"));

        std::fs::write(&file, r#"{"future": {"a": [1, 2]}, "statusline": "claude"}"#).unwrap();
        set_str(&file, "statusline", "builtin").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!((v["statusline"].as_str(), v["future"]["a"][1].as_i64()), (Some("builtin"), Some(2)), "other keys are untouched");

        std::fs::write(&file, "not json {{").unwrap();
        assert_eq!(get_str(&file, "statusline"), None, "an unreadable file reads as empty");
        assert!(set_str(&file, "statusline", "claude").is_err(), "but is never overwritten");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "not json {{");
    }
}
