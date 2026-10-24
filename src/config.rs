//! claude++'s own config file, kept apart from Claude Code's settings: `$XDG_CONFIG_HOME/claude++/config.json`
//! (else `~/.config/claude++/config.json`). It is a flat JSON object. Only keys claude++ knows are read, and
//! saving one key keeps every other key as it was, so hand-edits and future options survive.

use serde_json::{Map, Value};
use std::io;
use std::path::{Path, PathBuf};

/// Folder name under the XDG config and cache homes. Earlier versions used `claudepp`.
pub const DIR: &str = "claude++";

/// One-time move of `claudepp` folders to `claude++`, so saved config, profiles and caches carry over.
/// Does nothing when the new folder exists already (never merges or overwrites).
pub fn migrate_dirs() {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    for (var, rel) in [("XDG_CONFIG_HOME", ".config"), ("XDG_CACHE_HOME", ".cache")] {
        let Some(base) = std::env::var_os(var).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(rel))) else { continue };
        let (old, new) = (base.join("claudepp"), base.join(DIR));
        if old.is_dir() && !new.exists() {
            let _ = std::fs::rename(old, new);
        }
    }
}

pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(DIR).join("config.json"))
}

fn load(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| if let Value::Object(m) = v { Some(m) } else { None })
        .unwrap_or_default()
}

pub fn get_str(path: &Path, key: &str) -> Option<String> {
    load(path).get(key).and_then(Value::as_str).map(String::from)
}

pub fn set_str(path: &Path, key: &str, value: &str) -> io::Result<()> {
    let mut m = load(path);
    m.insert(key.into(), Value::String(value.into()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&Value::Object(m))? + "\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn old_folders_move_once_and_never_overwrite() {
        let root = std::env::temp_dir().join(format!("claudeplusplus-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (conf, cache) = (root.join("conf"), root.join("cache"));
        std::fs::create_dir_all(conf.join("claudepp")).unwrap();
        std::fs::write(conf.join("claudepp/config.json"), "{\"profile\":\"lean\"}").unwrap();
        std::fs::create_dir_all(cache.join("claudepp")).unwrap();
        std::fs::create_dir_all(cache.join("claude++")).unwrap();
        std::fs::write(cache.join("claudepp/old"), "x").unwrap();
        // SAFETY: single test touching these variables, set before any thread of ours reads them
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &conf);
            std::env::set_var("XDG_CACHE_HOME", &cache);
        }
        super::migrate_dirs();
        assert_eq!(get_str(&conf.join("claude++/config.json"), "profile").as_deref(), Some("lean"));
        assert!(!conf.join("claudepp").exists());
        assert!(cache.join("claudepp/old").exists(), "an existing new folder is left alone, nothing merged");
    }

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
        set_str(&file, "statusline", "claude").unwrap();
        assert_eq!(get_str(&file, "statusline").as_deref(), Some("claude"));
    }
}
