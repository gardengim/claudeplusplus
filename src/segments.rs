//! What claude++'s own status line shows: the parts ("segments") you switch on and off and put in the order
//! you like with `/statusline items`. The permission mode always comes first and cannot be turned off, since
//! it decides whether Claude asks before acting. The choice is saved in `config.json` as `statusline_items`,
//! a comma list in display order. Adding a part is an entry in [`ALL`] plus an arm in `ui::segment`.

use crate::panel::{View, row_with};
use crate::theme;
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};

pub struct Seg {
    pub id: &'static str,
    pub label: &'static str,
    pub help: &'static str,
}

pub const ALL: &[Seg] = &[
    Seg { id: "profile", label: "Profile", help: "the tab's profile, when it is not `full`" },
    Seg { id: "model", label: "Model", help: "model and effort (↯ for fast mode)" },
    Seg { id: "ctx", label: "Context", help: "tokens in the conversation and their share of the window" },
    Seg { id: "hit", label: "Cache hit", help: "share of the prompt served from the prompt cache" },
    Seg { id: "cache", label: "Cache clock", help: "minutes left before the prompt cache expires (◷ cold once it has)" },
    Seg { id: "cost", label: "Cost", help: "what this tab has cost, in dollars" },
    Seg { id: "limits", label: "Usage limits", help: "your 5-hour and 7-day plan usage, once Claude Code reports it" },
    Seg { id: "mem", label: "Memory", help: "memory of the claude processes plus claude++'s own" },
    Seg { id: "dir", label: "Folder", help: "the tab's working folder" },
    Seg { id: "branch", label: "Git branch", help: "the branch checked out in that folder" },
    Seg { id: "clock", label: "Clock", help: "the time of day" },
    Seg { id: "version", label: "Claude Code", help: "the version of Claude Code this tab runs (`/version` lists and pins)" },
];

pub const DEFAULT: &[&str] = &["profile", "model", "ctx", "hit", "cache", "cost", "limits", "mem"];

/// The segments named in a saved list, in its order; unknown names and repeats are dropped.
pub fn parse(s: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = vec![];
    for name in s.split(',') {
        if let Some(seg) = ALL.iter().find(|g| g.id == name.trim())
            && !out.contains(&seg.id)
        {
            out.push(seg.id);
        }
    }
    out
}

/// Every segment, the shown ones first in their order, then the hidden ones: the panel's rows.
pub fn rows(shown: &[&'static str]) -> Vec<&'static str> {
    shown.iter().copied().chain(ALL.iter().map(|g| g.id).filter(|id| !shown.contains(id))).collect()
}

#[derive(Default)]
pub struct Ui {
    pub sel: usize,
}

impl Ui {
    /// Move the selected row `delta` rows (wrapping).
    pub fn step(&mut self, delta: isize) {
        self.sel = (self.sel as isize + delta).rem_euclid(ALL.len() as isize) as usize;
    }

    /// Show or hide the selected segment; the cursor follows it. Hidden ones join the end of the shown.
    pub fn toggle(&mut self, shown: &mut Vec<&'static str>) {
        let id = rows(shown)[self.sel];
        match shown.iter().position(|s| *s == id) {
            Some(at) => drop(shown.remove(at)),
            None => shown.push(id),
        }
        self.sel = rows(shown).iter().position(|r| *r == id).unwrap_or(0);
    }

    /// Move the selected segment one place earlier or later among the shown ones (a hidden one cannot move).
    pub fn shift(&mut self, shown: &mut [&'static str], delta: isize) {
        if self.sel >= shown.len() {
            return;
        }
        let to = self.sel as isize + delta;
        if (0..shown.len() as isize).contains(&to) {
            shown.swap(self.sel, to as usize);
            self.sel = to as usize;
        }
    }
}

pub fn view(ui: &Ui, shown: &[&'static str]) -> View {
    let order = rows(shown);
    let sel = ui.sel.min(order.len() - 1);
    let body: Vec<Line> = order
        .iter()
        .enumerate()
        .map(|(n, id)| {
            let seg = ALL.iter().find(|g| g.id == *id).unwrap();
            let on = shown.contains(id);
            let at = shown.iter().position(|s| s == id).map_or("  ".to_string(), |p| format!("{:>2}", p + 1));
            row_with(
                n == sel,
                false,
                vec![
                    Span::styled(format!("{} ", if on { "[x]" } else { "[ ]" }), if on { theme::ok() } else { theme::dim() }),
                    Span::styled(format!("{at}  {:<14}", seg.label), if n == sel { theme::bold() } else { Default::default() }),
                    Span::styled(seg.help, theme::dim()),
                ],
            )
        })
        .collect();
    let tail = vec![Line::raw(""), Line::styled("The line under the message box changes as you go. The permission mode always comes first.", theme::dim())];
    let footer = theme::hints(&[("↑↓", "move"), ("space", "show/hide"), ("alt+↑↓", "reorder"), ("r", "reset"), ("esc", "close")]);
    View::new("Status line", body, sel, footer).with_tail(tail).rows(12, 16)
}

/// The branch checked out at `cwd` or above it (a worktree's `.git` file is followed), read from `.git/HEAD`
/// directly: no `git` process per redraw. A detached HEAD shows as its short hash.
pub fn branch(cwd: &str) -> Option<String> {
    let mut dir: Option<&Path> = Some(Path::new(cwd));
    while let Some(d) = dir {
        let dot = d.join(".git");
        let git: Option<PathBuf> = if dot.is_dir() {
            Some(dot)
        } else {
            std::fs::read_to_string(&dot).ok().and_then(|t| t.strip_prefix("gitdir:").map(|p| d.join(p.trim())))
        };
        if let Some(git) = git {
            let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
            let head = head.trim();
            return Some(match head.strip_prefix("ref: refs/heads/") {
                Some(b) => b.to_string(),
                None => head.chars().take(7).collect(),
            });
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_list_keeps_its_order_and_drops_unknown_names_and_repeats() {
        assert_eq!(parse("mem, model,nope,mem,ctx"), ["mem", "model", "ctx"]);
        assert!(parse("").is_empty(), "an empty list is a choice too: show nothing but the mode");
        assert_eq!(parse(&DEFAULT.join(",")), DEFAULT);
    }

    #[test]
    fn toggling_and_reordering_keep_the_cursor_on_the_same_part() {
        let mut shown = vec!["model", "ctx", "mem"];
        let mut ui = Ui { sel: 1 };
        ui.shift(&mut shown, -1);
        assert_eq!((shown.clone(), ui.sel), (vec!["ctx", "model", "mem"], 0));
        ui.shift(&mut shown, -1);
        assert_eq!(shown, ["ctx", "model", "mem"], "the top stays the top");
        ui.toggle(&mut shown);
        assert_eq!(shown, ["model", "mem"]);
        assert_eq!(rows(&shown)[ui.sel], "ctx", "hidden now, the cursor went with it");
        ui.toggle(&mut shown);
        assert_eq!(shown, ["model", "mem", "ctx"], "shown again at the end");
        assert_eq!(rows(&shown)[ui.sel], "ctx");
        ui.sel = rows(&shown).len() - 1;
        let before = shown.clone();
        ui.shift(&mut shown, -1);
        assert_eq!(shown, before, "a hidden one does not move");
    }

    #[test]
    fn the_branch_is_read_from_head_through_a_worktree_file_and_a_detached_head() {
        let dir = std::env::temp_dir().join(format!("cpp-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo/.git")).unwrap();
        std::fs::create_dir_all(dir.join("repo/src/deep")).unwrap();
        std::fs::write(dir.join("repo/.git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(branch(dir.join("repo/src/deep").to_str().unwrap()).as_deref(), Some("feature/x"), "found from a subfolder");
        std::fs::create_dir_all(dir.join("wt")).unwrap();
        std::fs::create_dir_all(dir.join("gitdirs/wt")).unwrap();
        std::fs::write(dir.join("wt/.git"), format!("gitdir: {}\n", dir.join("gitdirs/wt").display())).unwrap();
        std::fs::write(dir.join("gitdirs/wt/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch(dir.join("wt").to_str().unwrap()).as_deref(), Some("0123456"));
        assert_eq!(branch("/"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
