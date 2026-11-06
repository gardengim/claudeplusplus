//! Which Claude Code claude++ runs. The native installer keeps every version it fetched as a file in
//! `~/.local/share/claude/versions/<x.y.z>` and points `~/.local/bin/claude` at one of them. `/version` lists
//! them, and pins claude++ to one: new and restarted tabs run that file directly, with Claude Code's own
//! updater switched off for them, while your `claude` command is left as it is. Nothing is installed or
//! changed here (`claude install <x.y.z>` fetches a version); an install that keeps no such folder (npm,
//! Homebrew) shows no list and cannot pin.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    pub version: String,
    pub path: PathBuf,
}

/// Where the native installer keeps versions (`$XDG_DATA_HOME/claude/versions`, else `~/.local/share/claude/versions`).
pub fn versions_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::home_dir().map(|h| h.join(".local/share")))?;
    Some(base.join("claude/versions"))
}

/// `2.1.319` as numbers; anything else (a `.partial` download, a stray file) is not a version.
fn parse(name: &str) -> Option<[u64; 3]> {
    let mut it = name.split('.').map(|p| p.parse::<u64>().ok());
    let v = [it.next()??, it.next()??, it.next()??];
    it.next().is_none().then_some(v)
}

/// The versions in `dir`, oldest first.
pub fn installed_in(dir: &Path) -> Vec<Installed> {
    let mut v: Vec<(_, Installed)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            Some((parse(&name)?, Installed { version: name, path: e.path() }))
        })
        .collect();
    v.sort_by_key(|(n, _)| *n);
    v.into_iter().map(|(_, i)| i).collect()
}

/// The version `claude` on `PATH` (or `bin`, if it is a path) resolves to, when that is one of the installed files.
pub fn default_in(dir: &Path, bin: &str) -> Option<String> {
    let found = if bin.contains('/') {
        PathBuf::from(bin)
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(bin)).find(|p| p.is_file())?
    };
    let real = std::fs::canonicalize(found).ok()?;
    let dir = std::fs::canonicalize(dir).ok()?;
    (real.parent()? == dir).then(|| real.file_name()?.to_str().map(String::from))?
}

/// Whether `version` is on the line claude++ was verified on (`2.1` for `2.1.319`).
pub fn on_verified_line(version: &str, line: &str) -> bool {
    version.strip_prefix(line).is_some_and(|r| r.starts_with('.'))
}

/// The text `/version` shows. `pinned` is what claude++ was told to run, `running` the versions tabs report.
pub fn describe(list: &[Installed], default: Option<&str>, pinned: Option<&str>, verified_line: &str, running: &[(String, String)]) -> String {
    if list.is_empty() {
        return "no installed Claude Code versions found (the native installer keeps them in ~/.local/share/claude/versions); an npm or Homebrew install cannot be pinned from here".into();
    }
    let newest = list.last().map(|i| i.version.as_str());
    let mut out = String::from("Claude Code versions installed:");
    for i in list {
        let mut tags = vec![];
        if default == Some(&i.version) {
            tags.push("default on PATH");
        }
        if pinned == Some(&i.version) {
            tags.push("pinned for claude++");
        }
        if newest == Some(&i.version) {
            tags.push("newest");
        }
        if on_verified_line(&i.version, verified_line) {
            tags.push("verified line");
        }
        let at = if pinned == Some(&i.version) || (pinned.is_none() && default == Some(&i.version)) { "▸" } else { " " };
        out += &format!("\n  {at} {}{}", i.version, if tags.is_empty() { String::new() } else { format!("  ({})", tags.join(", ")) });
    }
    if !running.is_empty() {
        out += &format!("\nrunning now: {}", running.iter().map(|(t, v)| format!("{t} {v}")).collect::<Vec<_>>().join(", "));
    }
    out += "\n/version <x.y.z> pins claude++ to one (its updater is switched off for it), /version default goes back; `claude install <x.y.z>` fetches another";
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cpp-version-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn versions_sort_as_numbers_and_stray_files_are_not_versions() {
        let d = dir("list");
        for n in ["2.1.9", "2.1.10", "2.0.99", "2.1.10.partial", "latest", "2.1"] {
            std::fs::write(d.join(n), "x").unwrap();
        }
        std::fs::create_dir(d.join("2.9.9")).unwrap();
        let v: Vec<_> = installed_in(&d).into_iter().map(|i| i.version).collect();
        assert_eq!(v, ["2.0.99", "2.1.9", "2.1.10"], "10 comes after 9, the folder and the partial download are left out");
        assert!(installed_in(&d.join("nope")).is_empty());
    }

    #[test]
    fn the_default_is_the_installed_file_the_command_resolves_to() {
        let d = dir("default");
        std::fs::write(d.join("2.1.5"), "x").unwrap();
        let link = d.join("claude");
        std::os::unix::fs::symlink(d.join("2.1.5"), &link).unwrap();
        assert_eq!(default_in(&d, link.to_str().unwrap()).as_deref(), Some("2.1.5"));
        let other = dir("default-other").join("claude");
        std::fs::write(&other, "x").unwrap();
        assert_eq!(default_in(&d, other.to_str().unwrap()), None, "a claude from somewhere else is not one of ours");
    }

    #[test]
    fn the_list_marks_what_is_default_pinned_newest_and_verified() {
        let l = |v: &str| Installed { version: v.into(), path: PathBuf::from(v) };
        let list = [l("2.0.1"), l("2.1.5"), l("2.2.0")];
        let text = describe(&list, Some("2.2.0"), Some("2.1.5"), "2.1", &[("tab 1".into(), "2.1.5".into())]);
        assert!(text.contains("▸ 2.1.5  (pinned for claude++, verified line)"), "{text}");
        assert!(text.contains("  2.2.0  (default on PATH, newest)") && text.contains("running now: tab 1 2.1.5"), "{text}");
        assert!(describe(&list, Some("2.2.0"), None, "2.1", &[]).contains("▸ 2.2.0"), "unpinned: the default is the one in use");
        assert!(describe(&[], None, None, "2.1", &[]).contains("no installed"));
        assert!(on_verified_line("2.1.319", "2.1") && !on_verified_line("2.10.1", "2.1") && !on_verified_line("3.1.0", "2.1"));
    }
}
