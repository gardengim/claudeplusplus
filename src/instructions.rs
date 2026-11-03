//! Instruction files (`CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md`) that Claude Code loads for a directory, shown in a
//! checkbox panel (`/instructions`). Unticking a file or one of its markdown sections keeps it out of the
//! context of tabs started afterwards, without touching the file: the rule lives in claude++'s config
//! (`hidden`: path → section ids, `*` for the whole file).
//!
//! How it is applied: a hidden file is added to Claude Code's `claudeMdExcludes` (`--settings`); a file
//! with only some sections hidden is excluded too and a filtered copy is appended to the system prompt from
//! the cache folder. Nothing is sent to a model. A section is a heading and everything under it, children
//! included, and is named by its heading path, so a heading added later shows up ticked.

use crate::panel::{View, row_with};
use crate::theme;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Rough characters per token for these files (markdown with code); measured against Claude Code's own
/// count for a 150 KB file, it is closer to 3 than the usual 4.
const BYTES_PER_TOKEN: usize = 3;
/// Files Claude Code reads from the working directory and each ancestor. Checked against its own list
/// (`get_context_usage` → `memoryFiles`) on 2.1.312: no `AGENTS.md`, in the project or above it, even with no
/// `CLAUDE.md` beside it. Listing one here would be worse than useless: hiding part of it appends the rest to
/// the prompt, adding text Claude Code never sent. If a version starts reading it, add it back.
const NAMES: [&str; 3] = ["CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"];
pub const WHOLE: &str = "*";
const INTRO: &str = "(intro)";

pub type Rules = BTreeMap<String, BTreeSet<String>>;

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    /// Heading path, e.g. `Setup > Install`; repeats get ` #2`.
    pub id: String,
    pub title: String,
    pub depth: usize,
    /// Line range `[from, to)` including children.
    pub from: usize,
    pub to: usize,
    pub bytes: usize,
}

pub fn sections(text: &str) -> Vec<Section> {
    let lines: Vec<&str> = text.lines().collect();
    let mut heads: Vec<(usize, usize, String)> = vec![]; // (line, level, title)
    let mut fence: Option<&str> = None;
    for (n, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        let mark = if t.starts_with("```") { "```" } else if t.starts_with("~~~") { "~~~" } else { "" };
        match (fence, mark) {
            (None, m) if !m.is_empty() => fence = Some(m),
            (Some(f), m) if f == m => fence = None,
            (None, _) => {
                let level = l.chars().take_while(|c| *c == '#').count();
                if (1..=6).contains(&level) && l[level..].starts_with(' ') && !l[level..].trim().is_empty() {
                    heads.push((n, level, l[level..].trim().trim_end_matches('#').trim().to_string()));
                }
            }
            _ => {}
        }
    }
    let size = |a: usize, b: usize| lines[a..b].iter().map(|l| l.len() + 1).sum::<usize>();
    let mut out = vec![];
    let first = heads.first().map_or(lines.len(), |h| h.0);
    if lines[..first].iter().any(|l| !l.trim().is_empty()) {
        out.push(Section { id: INTRO.into(), title: INTRO.into(), depth: 0, from: 0, to: first, bytes: size(0, first) });
    }
    let (mut path, mut seen): (Vec<(usize, String)>, BTreeMap<String, usize>) = (vec![], BTreeMap::new());
    for (i, (line, level, title)) in heads.iter().enumerate() {
        while path.last().is_some_and(|(l, _)| l >= level) {
            path.pop();
        }
        let mut id = path.iter().map(|(_, t)| t.as_str()).chain([title.as_str()]).collect::<Vec<_>>().join(" > ");
        let n = seen.entry(id.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            id = format!("{id} #{n}");
        }
        path.push((*level, title.clone()));
        let to = heads[i + 1..].iter().find(|h| h.1 <= *level).map_or(lines.len(), |h| h.0);
        out.push(Section { id, title: title.clone(), depth: path.len() - 1, from: *line, to, bytes: size(*line, to) });
    }
    out
}

fn hidden(secs: &[Section], set: &BTreeSet<String>, i: usize) -> bool {
    if set.contains(WHOLE) || set.contains(&secs[i].id) {
        return true;
    }
    secs[..i].iter().any(|p| p.to >= secs[i].to && p.from <= secs[i].from && p.depth < secs[i].depth && set.contains(&p.id))
}

/// Prefix of a rule id meaning "keep the heading and first line only".
pub const BRIEF: &str = "~";

fn brief(secs: &[Section], set: &BTreeSet<String>, i: usize) -> bool {
    let is = |s: &Section| set.contains(&format!("{BRIEF}{}", s.id));
    is(&secs[i]) || secs[..i].iter().any(|p| p.to >= secs[i].to && p.from <= secs[i].from && p.depth < secs[i].depth && is(p))
}

/// `text` without the hidden sections, and with each brief section cut to its heading and first line.
#[cfg(test)]
pub fn filtered(text: &str, secs: &[Section], set: &BTreeSet<String>) -> String {
    filtered_at(text, secs, set, "")
}

/// Like [`filtered`], naming `path` in the note left where a section was cut so the model can read the rest.
pub fn filtered_at(text: &str, secs: &[Section], set: &BTreeSet<String>, path: &str) -> String {
    let at = if path.is_empty() { "the source file".to_string() } else { path.to_string() };
    if set.contains(WHOLE) {
        // One line instead of the file, so the model knows it exists and can still read it.
        return format!("*(hidden by claude++ to save context: read {at} if it is relevant)*\n");
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut drop = vec![false; lines.len()];
    let mut note: Vec<Option<String>> = vec![None; lines.len()];
    for (i, s) in secs.iter().enumerate() {
        if hidden(secs, set, i) {
            if !drop[s.from] {
                // Only the outermost hidden section leaves a pointer: its children are inside the lines it names.
                note[s.from] = Some(format!("*(section \"{}\" hidden by claude++ to save context: lines {}–{} of {at}; read them if relevant)*", s.title, s.from + 1, s.to));
            }
            drop[s.from..s.to].iter_mut().for_each(|d| *d = true);
        } else if brief(secs, set, i) && !drop[s.from] {
            let first = lines[s.from + 1..s.to].iter().find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).map_or("", |l| l.trim());
            let first: String = first.chars().take(200).collect();
            if secs[..i].iter().any(|p| p.to >= s.to && p.from <= s.from && p.depth < s.depth && brief(secs, set, secs.iter().position(|x| x.id == p.id).unwrap_or(0))) {
                drop[s.from..s.to].iter_mut().for_each(|d| *d = true); // inside a brief parent: gone
            } else {
                drop[s.from + 1..s.to].iter_mut().for_each(|d| *d = true);
                note[s.from] = Some(format!("{}\n{first}\n*(shortened by claude++; the full section is in {at}: read it if you need it)*", lines[s.from]));
            }
        }
    }
    let mut out = String::new();
    for (n, l) in lines.iter().enumerate() {
        if let Some(t) = &note[n] {
            out += t;
            out.push('\n');
        } else if !drop[n] {
            out += l;
            out.push('\n');
        }
    }
    out
}

/// Every rule in `a` and `b`.
pub fn merge(a: &Rules, b: &Rules) -> Rules {
    let mut out = a.clone();
    for (k, v) in b {
        out.entry(k.clone()).or_default().extend(v.iter().cloned());
    }
    out
}

/// Instruction files that exist for `cwd`: the user's own, then each directory from `cwd` up.
pub fn discover(cwd: &str) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Some(d) = crate::config::claude_dir() {
        out.push(d.join("CLAUDE.md"));
    }
    for dir in Path::new(cwd).ancestors() {
        out.extend(NAMES.iter().map(|n| dir.join(n)));
    }
    let mut seen = BTreeSet::new();
    out.retain(|p| p.is_file() && seen.insert(p.clone()));
    out
}

pub fn rules_path() -> Option<PathBuf> {
    crate::config::path()
}

pub fn load_rules(file: &Path) -> Rules {
    let Some(Value::Object(m)) = crate::config::get_value(file, "hidden") else { return Rules::new() };
    m.into_iter()
        .map(|(k, v)| (k, v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect::<BTreeSet<_>>()))
        .filter(|(_, v)| !v.is_empty())
        .collect()
}

pub fn save_rules(file: &Path, rules: &Rules) {
    let _ = crate::config::set_value(file, "hidden", json!(rules));
}

/// Adjust a launch for the hidden rules in `rules`: exclude those files from Claude Code's own
/// loading (`claudeMdExcludes`, put in `settings`, which the caller passes as one `--settings`) and append the filtered copies of the partly hidden ones (merged into any append prompt the
/// profile already sets, since Claude Code takes one). Does nothing when nothing is hidden.
pub fn apply(rules: &Rules, cache: &Path, args: &mut Vec<String>, settings: &mut Map<String, Value>) {
    let (mut excluded, mut copies) = (vec![], String::new());
    for (path, set) in rules {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let secs = sections(&text);
        let keep = filtered_at(&text, &secs, set, path);
        if keep == filtered_at(&text, &secs, &BTreeSet::new(), path) {
            continue;
        }
        excluded.push(path.clone());
        if set.contains(WHOLE) {
            copies += &keep; // the one-line pointer
        } else if !keep.trim().is_empty() {
            copies += &format!("# {path} (some sections hidden by claude++)\n\n{keep}\n");
        }
    }
    if excluded.is_empty() {
        return;
    }
    settings.insert("claudeMdExcludes".into(), json!(excluded));
    if copies.is_empty() {
        return;
    }
    let mut prior = String::new();
    while let Some(i) = args.iter().position(|a| a == "--append-system-prompt-file" || a == "--append-system-prompt") {
        let value = args.get(i + 1).cloned().unwrap_or_default();
        let file = args[i] == "--append-system-prompt-file";
        args.drain(i..(i + 2).min(args.len()));
        prior += &if file { std::fs::read_to_string(&value).unwrap_or_default() } else { value };
        prior += "\n\n";
    }
    let body = prior + &copies;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    let file = cache.join("instructions").join(format!("{:016x}.md", h.finish()));
    if !file.exists() && crate::config::write_atomic(&file, &body).is_err() {
        return;
    }
    args.extend(["--append-system-prompt-file".into(), file.to_string_lossy().into_owned()]);
}

pub struct Source {
    pub path: String,
    pub text: String,
    pub secs: Vec<Section>,
}

pub struct Ui {
    pub sel: usize,
    pub files: Vec<Source>,
    pub rules: Rules,
    pub open: BTreeSet<usize>,
    /// Something changed since the panel opened.
    pub dirty: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Row {
    File(usize),
    Sec(usize, usize),
}

impl Ui {
    pub fn new(cwd: &str, rules: Rules) -> Ui {
        let files = discover(cwd)
            .into_iter()
            .filter_map(|p| {
                let text = std::fs::read_to_string(&p).ok()?;
                Some(Source { path: p.to_string_lossy().into_owned(), secs: sections(&text), text })
            })
            .collect();
        Ui { sel: 0, files, rules, open: BTreeSet::new(), dirty: false }
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = vec![];
        for (f, src) in self.files.iter().enumerate() {
            out.push(Row::File(f));
            if self.open.contains(&f) {
                out.extend((0..src.secs.len()).map(|s| Row::Sec(f, s)));
            }
        }
        out
    }

    fn set(&self, f: usize) -> BTreeSet<String> {
        self.rules.get(&self.files[f].path).cloned().unwrap_or_default()
    }

    /// Bytes still sent, and bytes hidden, for one file.
    pub fn sizes(&self, f: usize) -> (usize, usize) {
        let src = &self.files[f];
        let keep = filtered_at(&src.text, &src.secs, &self.set(f), &src.path).len();
        (keep, src.text.len().saturating_sub(keep))
    }

    /// 0 none shown, 1 some, 2 all.
    fn state(&self, f: usize) -> u8 {
        if self.set(f).contains(WHOLE) {
            return 0; // only its pointer is sent
        }
        let src = &self.files[f];
        let keep = filtered_at(&src.text, &src.secs, &self.set(f), &src.path);
        // Compared with the unfiltered rendering, not the raw text, which may lack a final newline.
        if keep == filtered_at(&src.text, &src.secs, &BTreeSet::new(), &src.path) {
            2
        } else if keep.trim().is_empty() {
            0
        } else {
            1
        }
    }

    pub fn toggle(&mut self, row: Row) -> Result<(), &'static str> {
        let (Row::File(f) | Row::Sec(f, _)) = row;
        let mut cur = self.set(f);
        match row {
            Row::File(_) => {
                if self.state(f) == 2 { cur = [WHOLE.to_string()].into() } else { cur.clear() }
            }
            Row::Sec(_, s) => {
                let secs = &self.files[f].secs;
                if hidden(secs, &cur, s) && !cur.contains(WHOLE) && !cur.contains(&secs[s].id) {
                    return Err("hidden along with its parent section");
                }
                if cur.remove(WHOLE) {
                    // Everything else stays hidden; its parents must show, or they would hide it again.
                    let me = &secs[s];
                    cur = secs.iter().filter(|p| !(p.from <= me.from && p.to >= me.to && p.depth <= me.depth)).map(|x| x.id.clone()).collect();
                } else if !cur.remove(&secs[s].id) {
                    cur.insert(secs[s].id.clone());
                }
            }
        }
        let path = self.files[f].path.clone();
        if cur.is_empty() { self.rules.remove(&path) } else { self.rules.insert(path, cur) };
        self.dirty = true;
        Ok(())
    }

    /// Keep a section as its heading and first line (or go back to the full text).
    pub fn toggle_brief(&mut self, row: Row) {
        let Row::Sec(f, sidx) = row else { return };
        let mut cur = self.set(f);
        let (id, b) = (self.files[f].secs[sidx].id.clone(), format!("{BRIEF}{}", self.files[f].secs[sidx].id));
        if !cur.remove(&b) {
            cur.remove(&id);
            cur.remove(WHOLE);
            cur.insert(b);
        }
        let path = self.files[f].path.clone();
        if cur.is_empty() { self.rules.remove(&path) } else { self.rules.insert(path, cur) };
        self.dirty = true;
    }

    pub fn total(&self) -> (usize, usize) {
        (0..self.files.len()).map(|f| self.sizes(f)).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    }
}

fn est(bytes: usize) -> String {
    crate::usage::tokens((bytes / BYTES_PER_TOKEN) as u64)
}

pub fn view(ui: &Ui, width: usize) -> View {
    let rows = ui.rows();
    if rows.is_empty() {
        return View::new("Instructions", vec![Line::styled("  no CLAUDE.md applies to this directory", theme::dim())], 0, theme::hints(&[("esc", "close")]));
    }
    let sel = ui.sel.min(rows.len() - 1);
    let body = rows
        .iter()
        .enumerate()
        .map(|(n, row)| {
            let on = n == sel;
            let (mark, label, size) = match *row {
                Row::File(f) => {
                    let (keep, gone) = ui.sizes(f);
                    let m = match ui.state(f) { 2 => theme::ON, 1 => theme::PART, _ => theme::OFF };
                    let open = if ui.open.contains(&f) { theme::OPEN } else { theme::CLOSED };
                    let tail = if gone > 0 { format!("{} of {}", est(keep), est(keep + gone)) } else { est(keep) };
                    (m, format!("{open} {} · {}", crate::ui::tilde(&ui.files[f].path), ui.files[f].secs.len()), tail)
                }
                Row::Sec(f, s) => {
                    let sec = &ui.files[f].secs[s];
                    let off = hidden(&ui.files[f].secs, &ui.set(f), s);
                    let short = !off && brief(&ui.files[f].secs, &ui.set(f), s);
                    (if off { theme::OFF } else if short { theme::BRIEF } else { theme::ON }, format!("{}{}", "  ".repeat(sec.depth + 1), sec.title), est(sec.bytes))
                }
            };
            let off = mark == theme::OFF;
            let style = if on { theme::bold() } else if off { theme::dim() } else { Style::new() };
            let pad = width.saturating_sub(12).saturating_sub(label.chars().count()).saturating_sub(size.chars().count()).max(1);
            row_with(on, false, vec![Span::styled(format!("{mark} "), if off { theme::dim() } else if mark == theme::ON { theme::ok() } else { theme::accent() }), Span::styled(label.chars().take(width.saturating_sub(20)).collect::<String>(), style), Span::raw(" ".repeat(pad)), Span::styled(size, theme::dim())])
        })
        .collect();
    let (keep, gone) = ui.total();
    let mut tail = vec![Line::raw("")];
    tail.push(Line::from(vec![
        Span::styled(format!("≈{} tokens", est(keep)), theme::bold()),
        Span::styled(format!(" at session start{}", if gone > 0 { format!(" · {} hidden", est(gone)) } else { String::new() }), theme::dim()),
    ]));
    if ui.dirty {
        tail.push(Line::styled("applies to tabs started from now on", theme::dim()));
    }
    let side = match rows[sel] {
        Row::Sec(f, s) => {
            let sec = &ui.files[f].secs[s];
            ui.files[f].text.lines().skip(sec.from).take(sec.to.saturating_sub(sec.from).min(12)).map(|l| Line::styled(l.chars().take(70).collect::<String>(), theme::dim())).collect()
        }
        Row::File(f) => vec![Line::styled(ui.files[f].path.clone(), theme::dim())],
    };
    View::new("Instructions", body, sel, theme::hints(&[("↑↓", "move"), ("space", "show/hide"), ("b", "brief"), ("→←", "fold"), ("esc", "close")])).with_tail(tail).with_side(side).rows(0, 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "intro line\n# A\na text\n## A1\na1 text\n```\n# not a heading\n```\n## A2\na2 text\n# B\nb text\n# A\ndup\n";

    #[test]
    fn sections_nest_skip_fences_and_name_repeats() {
        let s = sections(DOC);
        let ids: Vec<_> = s.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["(intro)", "A", "A > A1", "A > A2", "B", "A #2"]);
        assert_eq!((s[1].from, s[1].to), (1, 10), "a parent holds its children");
        assert_eq!(s[2].depth, 1);
    }

    #[test]
    fn hiding_a_parent_hides_children_and_keeps_the_rest() {
        let s = sections(DOC);
        let set: BTreeSet<String> = ["A".to_string()].into();
        let out = filtered(DOC, &s, &set);
        assert!(out.contains("intro line") && out.contains("# B") && out.contains("dup"));
        assert!(!out.contains("a text") && !out.contains("a2 text") && !out.contains("not a heading"));
        let one: BTreeSet<String> = ["A > A1".to_string()].into();
        let out = filtered(DOC, &s, &one);
        assert!(out.contains("a text") && out.contains("a2 text") && !out.contains("a1 text"));
        let out = filtered_at(DOC, &s, &set, "/x/AGENTS.md");
        assert!(out.contains("*(section \"A\" hidden by claude++ to save context: lines 2–10 of /x/AGENTS.md; read them if relevant)*"), "{out}");
        assert_eq!(out.matches("hidden by claude++").count(), 1, "its children are inside the lines it names");
        let whole = filtered_at(DOC, &s, &[WHOLE.to_string()].into(), "/x/AGENTS.md");
        assert!(whole.contains("read /x/AGENTS.md") && !whole.contains("a text"), "a hidden file leaves one line saying where it is");
    }

    #[test]
    fn the_panel_draws_checkboxes_sizes_and_the_running_total() {
        let d = temp("view");
        std::fs::write(d.join("CLAUDE.md"), big()).unwrap();
        let mut ui = Ui::new(d.to_str().unwrap(), Rules::new());
        let f = ui.files.iter().position(|s| s.path.starts_with(d.to_str().unwrap())).unwrap();
        ui.open.insert(f);
        ui.toggle(Row::Sec(f, 4)).unwrap();
        let v = view(&ui, 80);
        let text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        let body: Vec<String> = v.body.iter().map(text).collect();
        assert!(body[f].contains("▣") && body[f + 1].contains("☑   (intro)") && body.iter().any(|l| l.contains("☐ ") && l.contains("B")), "{body:?}");
        assert!(v.tail.iter().any(|l| text(l).contains("hidden")));
    }

    #[test]
    fn a_brief_section_keeps_its_heading_and_first_line_and_points_at_the_file() {
        let s = sections(DOC);
        let set: BTreeSet<String> = ["~A".to_string()].into();
        let out = filtered_at(DOC, &s, &set, "/x/AGENTS.md");
        assert!(out.contains("# A\na text\n*(shortened by claude++; the full section is in /x/AGENTS.md"), "{out}");
        assert!(!out.contains("a1 text") && !out.contains("a2 text") && out.contains("# B") && out.contains("dup"));
        assert!(out.len() < DOC.len() + 90);
        // hiding wins over brief, and toggling brief twice goes back to the full text
        let both: BTreeSet<String> = ["~A".to_string(), "A".to_string()].into();
        assert!(!filtered(DOC, &s, &both).contains("# A\na text"));
        let d = temp("brief");
        std::fs::write(d.join("CLAUDE.md"), DOC).unwrap();
        let mut ui = Ui::new(d.to_str().unwrap(), Rules::new());
        let f = ui.files.iter().position(|x| x.path.starts_with(d.to_str().unwrap())).unwrap();
        ui.toggle_brief(Row::Sec(f, 1));
        assert!(ui.rules.values().next().unwrap().contains("~A"));
        ui.toggle_brief(Row::Sec(f, 1));
        assert!(ui.rules.is_empty());
    }

    /// DOC with sections the size of real ones: a hidden section leaves a pointer line, which only saves
    /// anything when the section is longer than it.
    fn big() -> String {
        DOC.replace(" text\n", &" text with enough words in it to outweigh the pointer line left behind\n".repeat(8))
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("claudeplusplus-instr-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn toggles_are_checkbox_like_and_whole_file_expands_on_a_section_toggle() {
        let d = temp("toggle");
        std::fs::write(d.join("CLAUDE.md"), big()).unwrap();
        let mut ui = Ui::new(d.to_str().unwrap(), Rules::new());
        let f = ui.files.iter().position(|s| s.path.starts_with(d.to_str().unwrap())).unwrap();
        assert_eq!(ui.state(f), 2);
        ui.toggle(Row::Sec(f, 4)).unwrap(); // hide B
        assert_eq!(ui.state(f), 1);
        assert!(ui.toggle(Row::Sec(f, 2)).is_ok());
        ui.toggle(Row::File(f)).unwrap(); // partial → all shown
        assert_eq!(ui.state(f), 2);
        assert!(ui.rules.is_empty());
        ui.toggle(Row::File(f)).unwrap(); // all → whole file hidden
        assert_eq!(ui.state(f), 0);
        ui.toggle(Row::Sec(f, 4)).unwrap(); // show B again: everything else stays hidden
        let (keep, gone) = ui.sizes(f);
        assert!(keep > 0 && gone > 0 && ui.state(f) == 1);
        ui.toggle(Row::File(f)).unwrap(); // partial → all shown again
        ui.toggle(Row::Sec(f, 1)).unwrap(); // hide A
        assert_eq!(ui.toggle(Row::Sec(f, 2)), Err("hidden along with its parent section"));
    }

    #[test]
    fn a_file_without_a_final_newline_counts_as_all_shown_and_a_child_can_be_shown_from_a_hidden_file() {
        let d = temp("nonl");
        std::fs::write(d.join("CLAUDE.md"), DOC.trim_end()).unwrap();
        let mut ui = Ui::new(d.to_str().unwrap(), Rules::new());
        let f = ui.files.iter().position(|s| s.path.starts_with(d.to_str().unwrap())).unwrap();
        assert_eq!((ui.state(f), ui.sizes(f).1), (2, 0), "no phantom hidden bytes, no underflow");
        let (mut args, mut settings) = (vec![], Map::new());
        apply(&[(ui.files[f].path.clone(), ["Z".to_string()].into())].into(), &d, &mut args, &mut settings);
        assert!(args.is_empty() && settings.is_empty(), "a rule that hides nothing excludes nothing");
        ui.toggle(Row::File(f)).unwrap(); // whole file hidden
        ui.toggle(Row::Sec(f, 2)).unwrap(); // show A > A1
        let src = &ui.files[f];
        let out = filtered_at(&src.text, &src.secs, &ui.set(f), "");
        assert!(out.contains("a1 text") && !out.contains("a2 text") && !out.contains("b text"), "{out}");
    }

    #[test]
    fn apply_excludes_and_appends_a_filtered_copy_merged_with_the_profile_prompt() {
        let d = temp("apply");
        let file = d.join("CLAUDE.md");
        std::fs::write(&file, DOC).unwrap();
        let path = file.to_string_lossy().into_owned();
        let mut args: Vec<String> = vec!["--append-system-prompt".into(), "from profile".into(), "--x".into()];
        let mut settings = Map::new();
        apply(&Rules::new(), &d, &mut args, &mut settings);
        assert!(args.len() == 3 && settings.is_empty(), "no rules, no change");
        let rules: Rules = [(path.clone(), ["B".to_string()].into())].into();
        apply(&rules, &d, &mut args, &mut settings);
        let at = |f: &str| args.iter().position(|a| a == f).unwrap();
        assert!(settings["claudeMdExcludes"].to_string().contains(&path));
        assert!(!args.contains(&"--append-system-prompt".to_string()), "profile prompt folded into the file");
        let body = std::fs::read_to_string(&args[at("--append-system-prompt-file") + 1]).unwrap();
        assert!(body.contains("from profile") && body.contains("a text") && !body.contains("b text"));
        // a whole-file rule excludes it and leaves only the line saying where it is
        let (mut args, mut settings) = (vec![], Map::new());
        apply(&[(path.clone(), [WHOLE.to_string()].into())].into(), &d, &mut args, &mut settings);
        let body = std::fs::read_to_string(&args[args.iter().position(|a| a == "--append-system-prompt-file").unwrap() + 1]).unwrap();
        assert!(settings.contains_key("claudeMdExcludes") && body.trim() == format!("*(hidden by claude++ to save context: read {path} if it is relevant)*"), "{body}");
    }
}
