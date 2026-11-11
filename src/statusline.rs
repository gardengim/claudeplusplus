//! Claude Code's custom status line, run the way Claude Code runs it: the `statusLine` command from the
//! settings (managed > local > project > user, project ones only in a folder Claude Code trusts) is started
//! in a shell, given the session's state as JSON on stdin, and its ANSI-coloured stdout is shown under the
//! input. Scripts of any language work because the command is run, not interpreted (a `#!/usr/bin/env
//! python3` script runs under python). Nothing stays resident: each refresh is a short-lived child with a
//! timeout, started only when something it could display has changed (see `App::pump_statusline`).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_LINES: usize = 4;
const MAX_OUTPUT: u64 = 64 * 1024;

/// Which status line claude++ shows. This is claude++'s own choice, separate from Claude Code's settings.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// Run the `statusLine` command from your Claude Code settings: rich, but a process per refresh.
    Claude,
    /// Only claude++'s own line: nothing is ever spawned.
    Builtin,
    /// Run this command instead (from `--statusline "<command>"`, never saved).
    Command(String),
}

impl Mode {
    /// `claude`, `builtin` (or `off`), anything else is a command; empty means "not specified".
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "" | "auto" => None,
            "claude" | "on" => Some(Mode::Claude),
            "builtin" | "off" => Some(Mode::Builtin),
            cmd => Some(Mode::Command(cmd.to_string())),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Mode::Claude => "claude",
            Mode::Builtin => "builtin",
            Mode::Command(_) => "command",
        }
    }
}

/// Which settings file a status line came from (highest precedence first).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Flag,
    Managed,
    Local,
    Project,
    User,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub command: String,
    /// `refreshInterval` from the settings, in seconds (minimum 1).
    pub interval: Option<Duration>,
    /// Extra columns of indentation (`padding`).
    pub padding: usize,
    pub source: Source,
}

pub struct Lookup {
    pub found: Option<Config>,
    /// A project or local status line that was skipped because Claude Code has not trusted this folder.
    pub untrusted: Option<(Source, String)>,
}

/// Where settings live on this machine; a parameter so tests need not touch the real environment.
pub struct Paths {
    pub home: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    pub claude_json: Option<PathBuf>,
    pub managed: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = std::env::home_dir();
        let config = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
        Paths {
            config_dir: crate::config::claude_dir(),
            claude_json: config.map(|c| c.join(".claude.json")).or_else(|| home.as_ref().map(|h| h.join(".claude.json"))),
            home,
            managed: if cfg!(target_os = "macos") {
                PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
            } else {
                PathBuf::from("/etc/claude-code/managed-settings.json")
            },
        }
    }
}

/// The folder whose `.claude/settings*.json` count as project settings: the nearest ancestor of `cwd`
/// (up to its git root, never the home folder itself) that has one.
fn project_root(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        if Some(dir) == home || dir.parent().is_none() {
            return None;
        }
        if dir.join(".claude/settings.json").is_file() || dir.join(".claude/settings.local.json").is_file() {
            return Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            return None;
        }
    }
    None
}

/// The status line Claude Code would use here: managed, then project-local, then project, then user
/// settings (the first that defines one wins). Project and local ones need the folder to be trusted.
pub fn lookup(cwd: &Path, paths: &Paths) -> Lookup {
    let root = project_root(cwd, paths.home.as_deref());
    let candidates = [
        (Source::Managed, Some(paths.managed.clone())),
        (Source::Local, root.as_ref().map(|r| r.join(".claude/settings.local.json"))),
        (Source::Project, root.as_ref().map(|r| r.join(".claude/settings.json"))),
        (Source::User, paths.config_dir.as_ref().map(|d| d.join("settings.json"))),
    ];
    let mut untrusted = None;
    for (source, path) in candidates {
        let Some(cfg) = path
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| parse_config(&v, source))
        else {
            continue;
        };
        if matches!(source, Source::Project | Source::Local) && !crate::trust::trusted(cwd, paths.claude_json.as_deref()) {
            untrusted.get_or_insert((source, cfg.command));
            continue;
        }
        return Lookup { found: Some(cfg), untrusted };
    }
    Lookup { found: None, untrusted }
}

pub fn parse_config(settings: &Value, source: Source) -> Option<Config> {
    let s = &settings["statusLine"];
    let command = s["command"].as_str().map(str::trim).filter(|c| !c.is_empty() && s["type"] == "command")?;
    let interval = s["refreshInterval"].as_u64().filter(|n| *n > 0).map(Duration::from_secs);
    let padding = s["padding"].as_u64().unwrap_or(0).min(20) as usize;
    Some(Config { command: command.to_string(), interval, padding, source })
}

/// The shell that runs the command. Scripts are mostly bash (`#!/bin/bash` or inline `jq` pipes), so the
/// user's login shell is used when it is bash or zsh, and bash otherwise (never fish: its syntax differs).
fn shell() -> String {
    shell_for(&std::env::var("SHELL").unwrap_or_default())
}

fn shell_for(login: &str) -> String {
    let name = login.rsplit('/').next().unwrap_or_default();
    if matches!(name, "bash" | "zsh") && Path::new(login).is_file() {
        return login.to_string();
    }
    ["/opt/homebrew/bin/bash", "/usr/local/bin/bash", "/usr/bin/bash", "/bin/bash"].into_iter().find(|p| Path::new(p).is_file()).unwrap_or("/bin/sh").to_string()
}

/// Run the command with `input` on stdin; its stdout lines, or None if it failed to start, timed out,
/// was cancelled or printed nothing. `COLUMNS`/`LINES` carry the terminal size, since the script's
/// output is captured and `tput` cannot see the terminal.
pub fn run(command: &str, input: &str, size: (u16, u16), cwd: &str, cancel: &AtomicBool) -> Option<Vec<String>> {
    let mut cmd = Command::new(shell());
    cmd.arg("-c").arg(command).env("COLUMNS", size.0.to_string()).env("LINES", size.1.to_string());
    if Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    // Its own process group, so a timeout kills the whole pipeline, not just the shell.
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0).spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let input = input.to_string();
    // A script that never reads stdin must not block us: write from a thread.
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.take(MAX_OUTPUT).read_to_string(&mut out);
        out
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < TIMEOUT && !cancel.load(Ordering::Relaxed) => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                // SAFETY: plain syscall; the group id is the child's pid (process_group(0) above).
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return None;
            }
        }
    }
    let text = reader.join().ok()?;
    let mut lines: Vec<String> = text.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.truncate(MAX_LINES);
    (!lines.is_empty()).then_some(lines)
}

/// Add what costs a little to learn (`git` runs, a transcript scan) to the session JSON. Called on the
/// worker thread, so the UI never waits for it.
pub fn enrich(json: &mut Value, cwd: &str) {
    let git = git_info(cwd);
    for k in ["repo", "git_worktree"] {
        if !git[k].is_null() {
            json["workspace"][k] = git[k].clone();
        }
    }
    let name = json["transcript_path"].as_str().and_then(|p| crate::history::session_name(Path::new(p)));
    if let Some(n) = name {
        json["session_name"] = json!(n);
    }
}

/// `workspace.repo` (host/owner/name of `origin`) and `workspace.git_worktree`, as Claude Code reports
/// them. Costs two short `git` runs, so the answer is cached for a minute per folder.
pub fn git_info(cwd: &str) -> Value {
    static CACHE: std::sync::Mutex<Option<(Instant, String, Value)>> = std::sync::Mutex::new(None);
    if let Some((at, dir, v)) = CACHE.lock().ok().as_deref().and_then(|c| c.as_ref())
        && dir == cwd
        && at.elapsed() < Duration::from_secs(60)
    {
        return v.clone();
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(["-C", cwd])
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let mut v = json!({});
    if let Some(repo) = git(&["remote", "get-url", "origin"]).and_then(|u| parse_remote(&u)) {
        v["repo"] = repo;
    }
    // A linked worktree's git dir differs from the repository's common dir.
    if let (Some(dir), Some(common), Some(top)) = (git(&["rev-parse", "--absolute-git-dir"]), git(&["rev-parse", "--git-common-dir"]), git(&["rev-parse", "--show-toplevel"])) {
        let common = Path::new(&common);
        let common = if common.is_absolute() { common.to_path_buf() } else { Path::new(cwd).join(common) };
        if std::fs::canonicalize(&dir).ok() != std::fs::canonicalize(&common).ok() {
            v["git_worktree"] = json!(Path::new(&top).file_name().map(|n| n.to_string_lossy().into_owned()));
        }
    }
    if let Ok(mut c) = CACHE.lock() {
        *c = Some((Instant::now(), cwd.to_string(), v.clone()));
    }
    v
}

/// `git@github.com:owner/name.git`, `https://host/group/sub/name`, `ssh://git@host/owner/name` → host/owner/name.
fn parse_remote(url: &str) -> Option<Value> {
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        rest.split_once('/')?
    } else {
        let rest = url.rsplit_once('@').map_or(url, |(_, r)| r);
        rest.split_once(':')?
    };
    let host = host.split(':').next()?;
    let (owner, name) = path.trim_start_matches('/').rsplit_once('/')?;
    (!host.is_empty() && !owner.is_empty() && !name.is_empty()).then(|| json!({"host": host, "owner": owner, "name": name}))
}

/// Turn one line of ANSI-coloured text into styled spans. Colours, bold/dim/italic/underline/reverse/strike
/// are kept; every other escape sequence (cursor moves, hyperlinks, titles) is dropped.
pub fn parse_line(s: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::new();
    let mut buf = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut fin = None;
                    for p in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&p) {
                            fin = Some(p);
                            break;
                        }
                        params.push(p);
                    }
                    if fin == Some('m') {
                        if !buf.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut buf), style));
                        }
                        style = apply_sgr(style, &params);
                    }
                }
                Some(']') => {
                    // OSC: skip to BEL or ST.
                    chars.next();
                    while let Some(p) = chars.next() {
                        if p == '\x07' || (p == '\x1b' && chars.peek() == Some(&'\\')) {
                            if p == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\t' => buf.push_str("    "),
            c if c.is_control() => {}
            c => buf.push(c),
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style));
    }
    Line::from(spans)
}

fn apply_sgr(mut st: Style, params: &str) -> Style {
    let codes: Vec<u32> = if params.is_empty() { vec![0] } else { params.split([';', ':']).map(|p| p.parse().unwrap_or(0)).collect() };
    let mut i = 0;
    while i < codes.len() {
        let c = codes[i];
        match c {
            0 => st = Style::new(),
            1 => st = st.add_modifier(Modifier::BOLD),
            2 => st = st.add_modifier(Modifier::DIM),
            3 => st = st.add_modifier(Modifier::ITALIC),
            4 => st = st.add_modifier(Modifier::UNDERLINED),
            7 => st = st.add_modifier(Modifier::REVERSED),
            9 => st = st.add_modifier(Modifier::CROSSED_OUT),
            22 => st = st.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => st = st.remove_modifier(Modifier::ITALIC),
            24 => st = st.remove_modifier(Modifier::UNDERLINED),
            27 => st = st.remove_modifier(Modifier::REVERSED),
            29 => st = st.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => st = st.fg(Color::Indexed((c - 30) as u8)),
            90..=97 => st = st.fg(Color::Indexed((c - 90 + 8) as u8)),
            40..=47 => st = st.bg(Color::Indexed((c - 40) as u8)),
            100..=107 => st = st.bg(Color::Indexed((c - 100 + 8) as u8)),
            39 => st.fg = None,
            49 => st.bg = None,
            38 | 48 => {
                let color = match codes.get(i + 1) {
                    Some(5) => codes.get(i + 2).map(|n| (Color::Indexed(*n as u8), 2)),
                    Some(2) => match (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4)) {
                        (Some(r), Some(g), Some(b)) => Some((Color::Rgb(*r as u8, *g as u8, *b as u8), 4)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((col, used)) = color {
                    st = if c == 38 { st.fg(col) } else { st.bg(col) };
                    i += used;
                }
            }
            _ => {}
        }
        i += 1;
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn modes_parse_from_flags_and_config_values() {
        assert_eq!(Mode::parse(""), None);
        assert_eq!(Mode::parse("auto"), None);
        assert_eq!(Mode::parse("claude"), Some(Mode::Claude));
        assert_eq!(Mode::parse("on"), Some(Mode::Claude));
        assert_eq!(Mode::parse("builtin"), Some(Mode::Builtin));
        assert_eq!(Mode::parse("off"), Some(Mode::Builtin));
        assert_eq!(Mode::parse(" ~/bin/my-line --x "), Some(Mode::Command("~/bin/my-line --x".into())));
    }

    #[test]
    fn config_comes_from_a_command_type_status_line_only() {
        let u = |v: Value| parse_config(&v, Source::User);
        let c = u(json!({"statusLine": {"type": "command", "command": " ~/.claude/statusline.py ", "refreshInterval": 60, "padding": 2}})).unwrap();
        assert_eq!((c.command.as_str(), c.interval, c.padding), ("~/.claude/statusline.py", Some(Duration::from_secs(60)), 2));
        assert_eq!(u(json!({"statusLine": {"type": "command", "command": "x", "refreshInterval": 1}})).unwrap().interval, Some(Duration::from_secs(1)), "the documented minimum is 1");
        let c = u(json!({"statusLine": {"type": "command", "command": "x"}})).unwrap();
        assert_eq!((c.interval, c.padding), (None, 0));
        assert!(u(json!({"statusLine": {"type": "text", "command": "x"}})).is_none());
        assert!(u(json!({"statusLine": {"type": "command", "command": "  "}})).is_none());
        assert!(u(json!({"model": "x"})).is_none());
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn sl(cmd: &str) -> String {
        format!("{{\"statusLine\": {{\"type\": \"command\", \"command\": \"{cmd}\"}}}}")
    }

    #[test]
    fn precedence_is_managed_local_project_user_and_project_ones_need_trust() {
        let root = std::env::temp_dir().join(format!("claudeplusplus-sl-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, repo) = (root.join("home"), root.join("home/work/repo"));
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = repo.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        let paths = Paths {
            home: Some(home.clone()),
            config_dir: Some(home.join(".claude")),
            claude_json: Some(home.join(".claude.json")),
            managed: root.join("managed.json"),
        };
        let cmd = |l: &Lookup| l.found.as_ref().map(|c| (c.command.clone(), c.source));

        assert!(lookup(&sub, &paths).found.is_none());
        write(&home.join(".claude/settings.json"), &sl("user-cmd"));
        assert_eq!(cmd(&lookup(&sub, &paths)), Some(("user-cmd".into(), Source::User)));

        // project settings from a subdirectory (found at the repo root) wait for trust, then win over user
        write(&repo.join(".claude/settings.json"), &sl("project-cmd"));
        let l = lookup(&sub, &paths);
        assert_eq!(cmd(&l), Some(("user-cmd".into(), Source::User)), "untrusted project command is not run");
        assert_eq!(l.untrusted, Some((Source::Project, "project-cmd".into())));
        write(&home.join(".claude.json"), &format!("{{\"projects\": {{\"{}\": {{\"hasTrustDialogAccepted\": true}}}}}}", repo.display()));
        assert_eq!(cmd(&lookup(&sub, &paths)), Some(("project-cmd".into(), Source::Project)), "trust on a parent folder counts");
        assert!(lookup(&sub, &paths).untrusted.is_none());

        write(&repo.join(".claude/settings.local.json"), &sl("local-cmd"));
        assert_eq!(cmd(&lookup(&sub, &paths)), Some(("local-cmd".into(), Source::Local)));
        write(&paths.managed, &sl("managed-cmd"));
        assert_eq!(cmd(&lookup(&sub, &paths)), Some(("managed-cmd".into(), Source::Managed)));

        // trust recorded as false, or a different folder: back to user-level
        let _ = std::fs::remove_file(&paths.managed);
        write(&home.join(".claude.json"), &format!("{{\"projects\": {{\"{}\": {{\"hasTrustDialogAccepted\": false}}}}}}", repo.display()));
        assert_eq!(cmd(&lookup(&sub, &paths)), Some(("user-cmd".into(), Source::User)));
        // the home folder is never "a project" even if it has .claude/settings.json
        assert_eq!(cmd(&lookup(&home, &paths)), Some(("user-cmd".into(), Source::User)));
    }

    #[test]
    fn inline_commands_get_a_posix_shell_even_when_the_login_shell_is_fish() {
        let fish = shell_for("/opt/homebrew/bin/fish");
        assert!(fish.ends_with("bash") || fish == "/bin/sh", "{fish}");
        assert_eq!(shell_for(&fish), fish, "a login shell that is bash stays");
        // a bash-only construct and an inline pipe both work
        let out = run("v=hello; [[ $v == h* ]] && echo \"yes $v\" | cat", "{}", (80, 24), "/", &no_cancel());
        assert!(matches!(out.as_deref(), Some([l]) if l == "yes hello") || shell_for("fish") == "/bin/sh", "{out:?}");
    }

    #[test]
    fn a_script_in_another_language_runs_through_its_shebang() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-sl-shebang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let py = dir.join("line.py");
        std::fs::write(&py, "#!/usr/bin/env python3\nimport json,sys\nd=json.load(sys.stdin)\nprint('py:' + d['model']['display_name'])\n").unwrap();
        Command::new("chmod").arg("+x").arg(&py).status().unwrap();
        let out = run(py.to_str().unwrap(), "{\"model\":{\"display_name\":\"Haiku\"}}", (80, 24), "/", &no_cancel());
        assert_eq!(out.unwrap(), ["py:Haiku"]);
    }

    #[test]
    fn cancelling_stops_a_running_script_at_once() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        let t = Instant::now();
        let h = std::thread::spawn(move || run("sleep 5; echo late", "{}", (80, 24), "/", &c));
        std::thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Relaxed);
        assert!(h.join().unwrap().is_none());
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[test]
    fn remotes_parse_into_host_owner_name() {
        let r = |u: &str| parse_remote(u).map(|v| (v["host"].as_str().unwrap().to_string(), v["owner"].as_str().unwrap().to_string(), v["name"].as_str().unwrap().to_string()));
        let want = Some(("github.com".into(), "anthropics".into(), "claude-code".into()));
        assert_eq!(r("git@github.com:anthropics/claude-code.git"), want);
        assert_eq!(r("https://github.com/anthropics/claude-code"), want);
        assert_eq!(r("ssh://git@github.com:22/anthropics/claude-code.git"), want);
        assert_eq!(r("https://user:tok@github.com/anthropics/claude-code.git/"), want);
        assert_eq!(r("git@gitlab.com:group/sub/proj.git"), Some(("gitlab.com".into(), "group/sub".into(), "proj".into())), "subgroups stay in owner");
        assert_eq!(r("/local/path/only"), None);
        assert_eq!(r("nonsense"), None);
    }

    #[test]
    fn git_info_sees_origin_and_linked_worktrees() {
        let root = std::env::temp_dir().join(format!("claudeplusplus-sl-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let sh = |dir: &Path, cmd: &str| {
            let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output().unwrap().status.success();
            assert!(ok, "{cmd}");
        };
        sh(&root, "git init -q main && cd main && git commit -q --allow-empty -m x && git remote add origin git@github.com:acme/app.git && git worktree add -q ../linked -b other");
        let main = git_info(root.join("main").to_str().unwrap());
        assert_eq!(main["repo"]["owner"], "acme");
        assert!(main["git_worktree"].is_null(), "the main checkout is not a linked worktree");
        let linked = git_info(root.join("linked").to_str().unwrap());
        assert_eq!(linked["git_worktree"], "linked");
        assert!(git_info("/tmp").get("repo").is_none());
    }

    #[test]
    fn ansi_colours_and_attributes_become_styles() {
        // the shape your script prints: bold magenta, dim magenta, reset, plain
        let l = parse_line("\x1b[1;35mHaiku\x1b[0m\x1b[2;35m:low\x1b[0m  plain \x1b[1;9;31mgone\x1b[0m");
        assert_eq!(text(&l), "Haiku:low  plain gone");
        let s = |i: usize| l.spans[i].style;
        assert_eq!(s(0).fg, Some(Color::Indexed(5)));
        assert!(s(0).add_modifier.contains(Modifier::BOLD));
        assert!(s(1).add_modifier.contains(Modifier::DIM));
        assert_eq!(s(2), Style::new());
        assert!(l.spans.last().unwrap().style.add_modifier.contains(Modifier::CROSSED_OUT | Modifier::BOLD));
        let l = parse_line("\x1b[38;5;208mA\x1b[38;2;1;2;3mB\x1b[48;5;4mC\x1b[90mD\x1b[39mE");
        assert_eq!(l.spans[0].style.fg, Some(Color::Indexed(208)));
        assert_eq!(l.spans[1].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(l.spans[2].style.bg, Some(Color::Indexed(4)));
        assert_eq!(l.spans[3].style.fg, Some(Color::Indexed(8)));
        assert_eq!(l.spans[4].style.fg, None);
    }

    #[test]
    fn other_escape_sequences_and_control_chars_are_dropped() {
        assert_eq!(text(&parse_line("a\x1b[2Kb\x1b[1;1Hc\x1b]0;title\x07d\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\r\x07e")), "abcdlinke");
        assert_eq!(text(&parse_line("tab\there")), "tab    here");
        assert_eq!(text(&parse_line("broken \x1b[3")), "broken ");
        assert_eq!(text(&parse_line("")), "");
    }

    #[test]
    fn runs_the_command_with_json_on_stdin_and_columns_in_the_environment() {
        let out = run("cat; echo; echo \"cols=$COLUMNS lines=$LINES pwd=$(pwd)\"", "{\"a\":1}", (77, 22), "/tmp", &no_cancel()).unwrap();
        assert_eq!(out[0], "{\"a\":1}");
        assert!(out[1].starts_with("cols=77 lines=22 pwd=") && out[1].contains("tmp"), "{out:?}");
    }

    #[test]
    fn output_is_capped_trimmed_and_failures_are_quiet() {
        assert_eq!(run("printf 'a\\nb\\n\\n\\n'", "{}", (80, 24), "/", &no_cancel()).unwrap(), ["a", "b"]);
        assert_eq!(run("seq 1 20", "{}", (80, 24), "/", &no_cancel()).unwrap().len(), MAX_LINES);
        assert!(run("true", "{}", (80, 24), "/", &no_cancel()).is_none(), "no output, nothing to show");
        assert_eq!(run("echo ok; exit 3", "{}", (80, 24), "/", &no_cancel()).unwrap(), ["ok"], "a non-zero exit with output is still shown");
        assert!(run("exec /definitely/not/a/command", "{}", (80, 24), "/", &no_cancel()).is_none());
    }

    #[test]
    fn a_hung_script_is_killed_and_one_that_ignores_stdin_cannot_block_us() {
        let t = Instant::now();
        assert!(run("sleep 30", "{}", (80, 24), "/", &no_cancel()).is_none());
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
        assert!(run("sleep 37.123 | cat", "{}", (80, 24), "/", &no_cancel()).is_none());
        let orphans = Command::new("pgrep").args(["-f", "sleep 37.123"]).output().unwrap();
        assert!(orphans.stdout.is_empty(), "the whole pipeline is killed, not only the shell");
        let big = "x".repeat(2_000_000); // far more than a pipe buffer holds
        let t = Instant::now();
        assert_eq!(run("echo fast", &big, (80, 24), "/", &no_cancel()).unwrap(), ["fast"]);
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}
