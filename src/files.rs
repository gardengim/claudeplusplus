//! The file index behind the `@` picker: every file under the working directory (as git sees it, or
//! a bounded directory walk outside a repo), and a small fuzzy matcher over it.

use std::path::Path;

const MAX_FILES: usize = 100_000;
const WALK_DEPTH: usize = 5;
const SKIP_DIRS: [&str; 6] = ["node_modules", "target", "Library", "__pycache__", "venv", "dist"];

/// Paths relative to `cwd`; directories end in `/`. Sorted, so the listing is stable.
pub fn list(cwd: &str) -> Vec<String> {
    let mut files = git_files(cwd).unwrap_or_else(|| walk(Path::new(cwd)));
    files.truncate(MAX_FILES);
    let mut dirs = std::collections::BTreeSet::new();
    for f in &files {
        for (i, _) in f.match_indices('/') {
            dirs.insert(format!("{}/", &f[..i]));
        }
    }
    files.extend(dirs);
    files.sort();
    files.dedup();
    files
}

fn git_files(cwd: &str) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let cwd_has_files = text.split('\0').any(|p| !p.is_empty());
    cwd_has_files.then(|| text.split('\0').filter(|p| !p.is_empty()).map(String::from).collect())
}

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                if depth < WALK_DEPTH {
                    queue.push_back((path, depth + 1));
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
            }
            if out.len() >= MAX_FILES {
                return out;
            }
        }
    }
    out
}

/// Up to `n` best matches for `query`. An empty query lists the top level, directories first.
pub fn rank(files: &[String], query: &str, n: usize) -> Vec<String> {
    if query.is_empty() {
        let mut top: Vec<&String> = files.iter().filter(|f| f.trim_end_matches('/').find('/').is_none()).collect();
        top.sort_by_key(|f| (!f.ends_with('/'), f.to_lowercase()));
        return top.into_iter().take(n).cloned().collect();
    }
    let mut hits: Vec<(i32, &String)> = files.iter().filter_map(|f| score(f, query).map(|s| (s, f))).collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    hits.into_iter().take(n).map(|h| h.1.clone()).collect()
}

/// Subsequence match, case-insensitive. Rewards runs, word starts and matches in the file name.
fn score(path: &str, query: &str) -> Option<i32> {
    let p: Vec<char> = path.to_lowercase().chars().collect();
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let base_start = path.trim_end_matches('/').rfind('/').map_or(0, |i| path[..=i].chars().count());
    let (mut qi, mut score, mut last) = (0, 0i32, None::<usize>);
    for (i, c) in p.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            let boundary = i == 0 || matches!(p[i - 1], '/' | '.' | '_' | '-' | ' ');
            score += 1 + if boundary { 8 } else { 0 } + if last == Some(i.wrapping_sub(1)) { 6 } else { 0 } + if i >= base_start { 3 } else { 0 };
            last = Some(i);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    let base: String = p[base_start..].iter().collect();
    let query_s: String = q.iter().collect();
    if base.trim_end_matches('/').starts_with(&query_s) {
        score += 30;
    } else if base.contains(&query_s) {
        score += 15;
    }
    Some(score - p.len() as i32 / 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_query_lists_top_level_with_directories_first() {
        let f = idx(&["README.md", "src/", "src/main.rs", "Cargo.toml", "bench/", "bench/run.py"]);
        assert_eq!(rank(&f, "", 10), ["bench/", "src/", "Cargo.toml", "README.md"]);
    }

    #[test]
    fn fuzzy_prefers_file_name_and_word_starts_over_scattered_letters() {
        let f = idx(&["src/main.rs", "src/session.rs", "src/markdown.rs", "docs/maintenance-notes.txt", "xmxaxixn.txt"]);
        assert_eq!(rank(&f, "main", 3)[0], "src/main.rs");
        assert_eq!(rank(&f, "ses", 3)[0], "src/session.rs");
        assert_eq!(rank(&f, "mdr", 3), ["src/markdown.rs"], "subsequence across the name");
        assert!(rank(&f, "zzz", 3).is_empty());
        assert_eq!(rank(&f, "SRC/MA", 2)[0], "src/main.rs", "case-insensitive, matches across the path");
    }

    #[test]
    fn lists_a_directory_with_parents_and_skips_junk() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for p in ["a/b/c.txt", "a/d.txt", "top.md", "node_modules/x/y.js", ".hidden/z"] {
            let path = dir.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        assert_eq!(list(dir.to_str().unwrap()), ["a/", "a/b/", "a/b/c.txt", "a/d.txt", "top.md"]);
    }
}
