//! Opening links and files: `/open` lists the URLs and existing file paths in this tab's conversation (newest first;
//! Enter opens one, `e` edits a file), and a click on a link or path in the conversation opens it. Only `http(s)`
//! links are opened. A file opens with the system's default program, except kinds that run code or install
//! things when opened (apps, scripts, installers), which are shown in the file manager instead.

use crate::panel::{View, row_with};
use crate::session::{Entry, Item};
use crate::theme;
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Url(String),
    Path(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub target: Target,
    /// `file.rs:42`'s 42, when the text gave one.
    pub line: Option<u32>,
}

impl Found {
    pub fn shown(&self) -> String {
        match &self.target {
            Target::Url(u) => u.clone(),
            Target::Path(p) => format!("{}{}", crate::ui::tilde(&p.to_string_lossy()), self.line.map_or(String::new(), |l| format!(":{l}"))),
        }
    }
}

fn is_delim(c: char) -> bool {
    c.is_whitespace() || "\"'`<>[](){}|".contains(c)
}

/// What a word means as a link or path, or nothing. A path has to exist (relative ones from `cwd`).
pub fn parse_token(tok: &str, cwd: &Path) -> Option<Found> {
    let tok = tok.trim_start_matches('*').trim_end_matches(['.', ',', ';', ':', '!', '?', '*']);
    if tok.len() > 2000 {
        return None;
    }
    if (tok.starts_with("http://") || tok.starts_with("https://")) && tok.len() > 8 {
        return Some(Found { target: Target::Url(tok.to_string()), line: None });
    }
    if tok.contains("://") || tok.contains('=') || tok.contains('@') {
        return None;
    }
    // `path:12` and `path:12:3`
    let (mut path, mut line) = (tok, None);
    for _ in 0..2 {
        if let Some((head, tail)) = path.rsplit_once(':')
            && let Ok(n) = tail.parse::<u32>()
        {
            (path, line) = (head, Some(n));
        }
    }
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).is_some_and(|e| !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()));
    if path.is_empty() || !(path.contains('/') || ext) {
        return None;
    }
    let full = if let Some(rest) = path.strip_prefix("~/") {
        std::env::home_dir()?.join(rest)
    } else if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    full.exists().then_some(Found { target: Target::Path(full), line })
}

/// Every link and existing path in `text`, in order of appearance.
pub fn scan(text: &str, cwd: &Path, out: &mut Vec<Found>) {
    for tok in text.split(is_delim).filter(|t| t.len() > 1).take(2000) {
        if let Some(f) = parse_token(tok, cwd) {
            out.push(f);
        }
    }
}

/// The links and paths of a conversation, newest first, each once: what was said, what Claude read or changed, what you wrote.
pub fn collect(items: &[Item], cwd: &Path) -> Vec<Found> {
    let mut out: Vec<Found> = vec![];
    for item in items.iter().rev() {
        let mut here = vec![];
        match &item.entry {
            Entry::Assistant { text, .. } | Entry::User(text) => scan(text, cwd, &mut here),
            Entry::Tool { input, .. } => {
                for key in ["file_path", "path", "notebook_path"] {
                    if let Some(p) = input[key].as_str().and_then(|p| parse_token(p, cwd)) {
                        here.push(p);
                    }
                }
            }
            _ => {}
        }
        for f in here {
            if !out.iter().any(|o| o.target == f.target) {
                out.push(f);
            }
        }
        if out.len() >= 60 {
            break;
        }
    }
    out
}

/// The word at cell (`x`, `y`) of the drawn rows (a wide glyph's second cell is ""), when it is a link or path. A link that wraps
/// is put back together from the rows around it.
pub fn at(rows: &[Vec<String>], x: usize, y: usize, cwd: &Path) -> Option<Found> {
    let chars_of = |r: &Vec<String>| -> (Vec<char>, Vec<usize>) {
        let (mut cs, mut at) = (vec![], vec![]);
        for (cell, s) in r.iter().enumerate() {
            for c in s.chars() {
                cs.push(c);
                at.push(cell);
            }
        }
        (cs, at)
    };
    let (row, cell_of) = chars_of(rows.get(y)?);
    let i = cell_of.iter().position(|&c| c >= x)?;
    if is_delim(*row.get(i)?) {
        return None;
    }
    let (mut a, mut b) = (i, i);
    while a > 0 && !is_delim(row[a - 1]) {
        a -= 1;
    }
    while b + 1 < row.len() && !is_delim(row[b + 1]) {
        b += 1;
    }
    let mut tok: String = row[a..=b].iter().collect();
    // A long link is broken at the edge: take the run that continues it on the rows below, or above.
    let mut down = b + 1 == row.len();
    for r in rows.iter().skip(y + 1).take(3) {
        if !down {
            break;
        }
        let (r, _) = chars_of(r);
        let start = r.iter().take_while(|c| c.is_whitespace()).count();
        let run: String = r[start..].iter().take_while(|c| !is_delim(**c)).collect();
        down = !run.is_empty() && start + run.chars().count() == r.len();
        tok.push_str(&run);
    }
    let mut up = a == 0;
    for r in rows[..y].iter().rev().take(3) {
        if !up {
            break;
        }
        let (r, _) = chars_of(r);
        let run: Vec<char> = r.iter().rev().take_while(|c| !is_delim(**c)).copied().collect();
        up = !run.is_empty() && run.len() == r.len();
        tok.insert_str(0, &run.into_iter().rev().collect::<String>());
    }
    parse_token(&tok, cwd).or_else(|| parse_token(&row[a..=b].iter().collect::<String>(), cwd))
}

/// Kinds of file that do something when opened rather than show something.
const RISKY: &[&str] = &["app", "command", "sh", "bash", "zsh", "fish", "csh", "tool", "pkg", "dmg", "mpkg", "exe", "msi", "bat", "cmd", "ps1", "jar", "scpt", "workflow", "action", "terminal", "webloc", "inetloc", "desktop", "appimage", "run", "bin", "py", "pl", "rb", "js"];

/// Arguments to the system's opener for `f`, and what to tell the user.
pub fn plan(f: &Found) -> (Vec<String>, String) {
    let mac = cfg!(target_os = "macos");
    match &f.target {
        Target::Url(u) => (vec![if mac { "open" } else { "xdg-open" }.into(), u.clone()], format!("opening {u}")),
        Target::Path(p) => {
            let s = p.to_string_lossy().into_owned();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            if p.is_file() && RISKY.contains(&ext.as_str()) {
                let args = if mac { vec!["open".into(), "-R".into(), s.clone()] } else { vec!["xdg-open".into(), p.parent().map_or("/".into(), |d| d.to_string_lossy().into_owned())] };
                (args, format!("{} does things when opened: shown in the file manager instead (e edits it here)", crate::ui::tilde(&s)))
            } else {
                (vec![if mac { "open" } else { "xdg-open" }.into(), s.clone()], format!("opening {}", crate::ui::tilde(&s)))
            }
        }
    }
}

#[cfg(test)]
pub static OPENED: std::sync::Mutex<Vec<Vec<String>>> = std::sync::Mutex::new(Vec::new());

/// Run the opener without waiting for it. Under test the command is only recorded.
pub fn run(args: Vec<String>) -> Result<(), String> {
    #[cfg(test)]
    {
        OPENED.lock().unwrap().push(args);
        Ok(())
    }
    #[cfg(not(test))]
    {
        let mut c = std::process::Command::new(&args[0]);
        c.args(&args[1..]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let mut child = c.spawn().map_err(|e| format!("{}: {e}", args[0]))?;
        std::thread::spawn(move || drop(child.wait()));
        Ok(())
    }
}

#[derive(Default)]
pub struct Ui {
    pub sel: usize,
    pub found: Vec<Found>,
}

pub fn view(ui: &Ui) -> View {
    let sel = ui.sel.min(ui.found.len().saturating_sub(1));
    let body: Vec<Line> = if ui.found.is_empty() {
        vec![Line::styled("no links or existing files in this conversation yet", theme::dim())]
    } else {
        ui.found
            .iter()
            .enumerate()
            .map(|(n, f)| {
                let mark = match &f.target {
                    Target::Url(_) => "↗ ",
                    Target::Path(p) if p.is_dir() => "▸ ",
                    Target::Path(_) => "▤ ",
                };
                row_with(n == sel, false, vec![Span::styled(mark, theme::dim()), Span::styled(f.shown(), if n == sel { theme::bold() } else { Default::default() })])
            })
            .collect()
    };
    let footer = theme::hints(&[("↑↓", "move"), ("enter", "open"), ("e", "edit"), ("esc", "close")]);
    View::new("Open", body, sel, footer).rows(4, 14)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cpp-open-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/main.rs"), "x").unwrap();
        std::fs::write(d.join("README.md"), "x").unwrap();
        d
    }

    #[test]
    fn links_and_existing_paths_are_found_with_punctuation_and_line_numbers_taken_off() {
        let d = tmp("scan");
        let mut out = vec![];
        scan("See https://example.com/a?b=1, and (src/main.rs:42:7). Also README.md. Not `nothere.rs`, nor ftp://x.y/z or 3.5 or a.b.", &d, &mut out);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0].target, Target::Url("https://example.com/a?b=1".into()));
        assert_eq!((out[1].target.clone(), out[1].line), (Target::Path(d.join("src/main.rs")), Some(42)));
        assert_eq!(out[2].target, Target::Path(d.join("README.md")));
        assert!(parse_token("http://", &d).is_none() && parse_token("src", &d).is_none(), "a bare word is not a path");
    }

    #[test]
    fn a_click_finds_the_word_under_it_and_rejoins_a_wrapped_link() {
        let d = tmp("at");
        let rows = |lines: &[&str]| -> Vec<Vec<String>> { lines.iter().map(|l| l.chars().map(String::from).collect()).collect() };
        let r = rows(&["see src/main.rs now", "go to https://example.com/very/lo", "ng/path/here and more   "]);
        assert_eq!(at(&r, 6, 0, &d).map(|f| f.target), Some(Target::Path(d.join("src/main.rs"))));
        assert!(at(&r, 1, 0, &d).is_none(), "the word 'see' is nothing");
        assert!(at(&r, 3, 0, &d).is_none(), "nor is the space");
        let want = Some(Target::Url("https://example.com/very/long/path/here".into()));
        assert_eq!(at(&r, 12, 1, &d).map(|f| f.target), want, "clicked on the first part");
        assert_eq!(at(&r, 2, 2, &d).map(|f| f.target), want, "clicked on the continuation");
        // a wide glyph's second cell is empty and does not shift the columns
        let w = vec![vec!["日".to_string(), String::new(), " ".into(), "R".into(), "E".into(), "A".into(), "D".into(), "M".into(), "E".into(), ".".into(), "m".into(), "d".into()]];
        assert_eq!(at(&w, 8, 0, &d).map(|f| f.target), Some(Target::Path(d.join("README.md"))));
    }

    #[test]
    fn what_opens_is_a_link_or_a_file_but_code_that_would_run_is_only_shown() {
        let d = tmp("plan");
        std::fs::write(d.join("run.sh"), "x").unwrap();
        let (args, _) = plan(&Found { target: Target::Url("https://x.dev".into()), line: None });
        assert_eq!(args[1], "https://x.dev");
        let (args, _) = plan(&Found { target: Target::Path(d.join("README.md")), line: None });
        assert_eq!(args.last().unwrap(), &d.join("README.md").to_string_lossy());
        let (args, said) = plan(&Found { target: Target::Path(d.join("run.sh")), line: None });
        assert!(said.contains("file manager") && (args.contains(&"-R".to_string()) || !cfg!(target_os = "macos")), "{args:?}");
    }

    #[test]
    fn the_list_is_newest_first_without_repeats_and_includes_files_claude_touched() {
        let d = tmp("collect");
        let item = |entry| Item { entry, cache: None, flipped: false };
        let items = vec![
            item(Entry::Assistant { text: "old https://a.dev and README.md".into(), msg_id: String::new(), streaming: false }),
            item(Entry::Tool { id: "t".into(), name: "Edit".into(), input: serde_json::json!({"file_path": d.join("src/main.rs")}), result: None, nested: false }),
            item(Entry::Assistant { text: "new https://b.dev and https://a.dev".into(), msg_id: String::new(), streaming: false }),
        ];
        let got: Vec<String> = collect(&items, &d).iter().map(Found::shown).collect();
        assert_eq!(got[..2], ["https://b.dev", "https://a.dev"], "newest message first, its links in the order written: {got:?}");
        assert_eq!(got.len(), 4, "{got:?}");
        assert!(got.iter().any(|g| g.ends_with("src/main.rs")) && got.contains(&"https://b.dev".to_string()));
    }
}
