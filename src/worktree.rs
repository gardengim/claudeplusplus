//! Git worktrees, so parallel work on one repository gets a folder (and a tab) each. `/worktree` lists the repository's
//! worktrees with the tabs that are in them; Enter opens one in a tab, `n` adds one, `d` removes one. `git` does the work;
//! nothing is forced: git refuses to remove a worktree with changes in it, and that refusal is shown as it is.
//! A new worktree is a sibling folder `<repo>-<name>` on a branch `<name>` (created, or reused if it exists).

use crate::panel::{View, row_with};
use crate::theme;
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq)]
pub struct Wt {
    pub path: String,
    pub branch: Option<String>,
    pub head: String,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
}

/// `git worktree list --porcelain`. The first entry is the main worktree.
pub fn parse(text: &str) -> Vec<Wt> {
    text.split("\n\n")
        .filter_map(|block| {
            let mut w = Wt { path: String::new(), branch: None, head: String::new(), bare: false, locked: false, prunable: false };
            for line in block.lines() {
                let (key, val) = line.split_once(' ').unwrap_or((line, ""));
                match key {
                    "worktree" => w.path = val.to_string(),
                    "HEAD" => w.head = val.chars().take(7).collect(),
                    "branch" => w.branch = Some(val.strip_prefix("refs/heads/").unwrap_or(val).to_string()),
                    "bare" => w.bare = true,
                    "locked" => w.locked = true,
                    "prunable" => w.prunable = true,
                    _ => {}
                }
            }
            (!w.path.is_empty()).then_some(w)
        })
        .collect()
}

fn git(cwd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).stdin(std::process::Stdio::null()).output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("git failed").trim_start_matches("fatal: ").to_string())
    }
}

pub fn list(cwd: &str) -> Result<Vec<Wt>, String> {
    git(cwd, &["worktree", "list", "--porcelain"]).map(|t| parse(&t)).and_then(|l| if l.is_empty() { Err("not a git repository".into()) } else { Ok(l) })
}

/// A branch / folder name: letters, digits, `.`, `_`, `-` and `/` in the branch. No leading `-` or `.`, no `..`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with(['-', '.', '/'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
}

/// Where a worktree called `name` goes: next to the main one, as `<repo>-<name>` (slashes become dashes).
pub fn path_for(main: &str, name: &str) -> PathBuf {
    let main = Path::new(main);
    let repo = main.file_name().map_or("repo".to_string(), |n| n.to_string_lossy().into_owned());
    main.parent().unwrap_or(Path::new("/")).join(format!("{repo}-{}", name.replace('/', "-")))
}

/// Add a worktree for branch `name` (made from `base`, or HEAD, unless the branch exists). Returns its path.
pub fn add(cwd: &str, name: &str, base: Option<&str>) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!("{name:?} is not a usable name: letters, digits and . _ - / only"));
    }
    let all = list(cwd)?;
    let path = path_for(&all[0].path, name);
    if path.exists() {
        return Err(format!("{} already exists", crate::ui::tilde(&path.to_string_lossy())));
    }
    let p = path.to_string_lossy().into_owned();
    let exists = git(cwd, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_ok();
    if exists {
        git(cwd, &["worktree", "add", &p, name])?;
    } else {
        let mut args = vec!["worktree", "add", "-b", name, &p];
        args.extend(base);
        git(cwd, &args)?;
    }
    Ok(p)
}

/// Remove a worktree (never forced: git says no when there are changes). The main one cannot be removed.
pub fn remove(cwd: &str, path: &str) -> Result<(), String> {
    let all = list(cwd)?;
    if all[0].path == path {
        return Err("that is the main worktree".into());
    }
    git(cwd, &["worktree", "remove", path]).map(drop)
}

#[derive(Default)]
pub struct Ui {
    pub sel: usize,
    pub list: Vec<Wt>,
    /// The name being typed on the "new" row.
    pub adding: Option<String>,
    /// `d` was pressed on this row; pressing it again removes the worktree.
    pub armed: Option<usize>,
}

/// `tabs`: the folders the open tabs work in, to mark the worktrees that have one.
pub fn view(ui: &Ui, tabs: &[String], here: &str) -> View {
    let sel = ui.sel.min(ui.list.len().saturating_sub(1));
    let label_w = ui.list.iter().map(|w| w.path.chars().count().min(60)).max().unwrap_or(10).min(60);
    let mut body: Vec<Line> = ui
        .list
        .iter()
        .enumerate()
        .map(|(n, w)| {
            let shown = crate::ui::tilde(&w.path);
            let shown = if shown.chars().count() > label_w + 2 { format!("…{}", shown.chars().skip(shown.chars().count() - label_w).collect::<String>()) } else { shown };
            let branch = match (&w.branch, w.bare) {
                (_, true) => "bare".to_string(),
                (Some(b), _) => b.clone(),
                (None, _) => format!("detached {}", w.head),
            };
            let tabs_here = tabs.iter().filter(|t| *t == &w.path).count();
            let mut tags = vec![];
            if n == 0 {
                tags.push("main".to_string());
            }
            if w.path == here {
                tags.push("this tab".to_string());
            } else if tabs_here > 0 {
                tags.push(format!("{tabs_here} tab{}", if tabs_here > 1 { "s" } else { "" }));
            }
            if w.locked {
                tags.push("locked".into());
            }
            if w.prunable {
                tags.push("folder missing".into());
            }
            if ui.armed == Some(n) {
                tags.push("d again removes it".into());
            }
            row_with(
                n == sel,
                false,
                vec![
                    Span::styled(format!("{shown:<w$}  ", w = label_w + 2), if n == sel { theme::bold() } else { Default::default() }),
                    Span::styled(format!("{branch:<24}"), theme::accent()),
                    Span::styled(tags.join(" · "), if ui.armed == Some(n) { theme::danger() } else { theme::dim() }),
                ],
            )
        })
        .collect();
    if let Some(name) = &ui.adding {
        body.push(Line::from(vec![Span::styled("new worktree and branch: ", theme::bold()), Span::raw(name.clone()), Span::styled("▏", theme::accent())]));
    }
    let footer = if ui.adding.is_some() {
        theme::hints(&[("enter", "create"), ("esc", "cancel")])
    } else {
        theme::hints(&[("↑↓", "move"), ("enter", "open in a tab"), ("c", "cd this tab"), ("n", "new"), ("d", "remove"), ("esc", "close")])
    };
    View::new("Worktrees", body, sel, footer).rows(4, 14)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_output_is_read_into_worktrees() {
        let text = "worktree /r/app\nHEAD 0123456789abcdef\nbranch refs/heads/main\n\nworktree /r/app-x\nHEAD abcdef0123456789\ndetached\nlocked reason\n\nworktree /r/gone\nHEAD 1111111111\nbranch refs/heads/old\nprunable gitdir file points to non-existent location\n";
        let l = parse(text);
        assert_eq!(l.len(), 3);
        assert_eq!((l[0].path.as_str(), l[0].branch.as_deref(), l[0].head.as_str()), ("/r/app", Some("main"), "0123456"));
        assert!(l[1].branch.is_none() && l[1].locked && !l[1].prunable);
        assert!(l[2].prunable);
    }

    #[test]
    fn names_are_checked_and_paths_sit_next_to_the_main_worktree() {
        for ok in ["fix", "feat/login", "v1.2_x-y"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-rf", ".hidden", "a..b", "a b", "a;b", "/abs", "a//b", "x/", "$(x)", "é"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(path_for("/work/app", "feat/login"), PathBuf::from("/work/app-feat-login"));
    }

    fn repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cpp-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let r = d.join("app");
        std::fs::create_dir_all(&r).unwrap();
        let g = |args: &[&str]| {
            let ok = Command::new("git").current_dir(&r).args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main"]).args(args).output().unwrap().status.success();
            assert!(ok, "git {args:?}");
        };
        g(&["init", "-q"]);
        std::fs::write(r.join("a.txt"), "x").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "one"]);
        r
    }

    #[test]
    fn a_worktree_is_added_listed_refused_when_dirty_and_removed() {
        if Command::new("git").arg("--version").output().is_err() {
            return eprintln!("skipped: no git");
        }
        let r = repo("flow");
        let cwd = r.to_string_lossy().into_owned();
        let p = add(&cwd, "feat/x", None).unwrap();
        assert!(Path::new(&p).join("a.txt").exists() && p.ends_with("app-feat-x"));
        let l = list(&cwd).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].branch.as_deref(), Some("feat/x"));
        assert!(l[0].path.ends_with("/app") && l[1].path.ends_with("app-feat-x"), "{l:?}");
        // the same name again: the folder exists
        assert!(add(&cwd, "feat/x", None).unwrap_err().contains("already exists"));
        // an existing branch is reused
        std::fs::remove_dir_all(&p).unwrap();
        let _ = git(&cwd, &["worktree", "prune"]);
        assert!(add(&cwd, "feat/x", None).is_ok(), "the branch exists now and is checked out again");
        // uncommitted work: git refuses and we do not force
        std::fs::write(Path::new(&p).join("new.txt"), "dirty").unwrap();
        assert!(remove(&cwd, &p).is_err() && Path::new(&p).exists());
        std::fs::remove_file(Path::new(&p).join("new.txt")).unwrap();
        remove(&cwd, &p).unwrap();
        assert_eq!(list(&cwd).unwrap().len(), 1);
        assert!(remove(&cwd, &list(&cwd).unwrap()[0].path).unwrap_err().contains("main"));
        assert!(add(&cwd, "bad name", None).is_err());
        assert!(list("/").is_err(), "not a repository");
        let _ = std::fs::remove_dir_all(r.parent().unwrap());
    }
}
