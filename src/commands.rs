//! Sending: `/` commands claude++ handles itself, `!` shell commands, and the hand-off to the session.

use super::*;

impl App {
    pub(crate) fn submit(&mut self) {
        if self.sessions[self.active].rewinding {
            return self.flash("restoring files; send once they are back");
        }
        if self.sessions[self.active].loading {
            return self.flash("still reading the conversation; send in a moment");
        }
        self.reset_undo = true;
        if self.vim.on {
            self.vim.mode = vim::Mode::Insert; // sent: type the next one
        }
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_pos = None;
        if text.trim().is_empty() && self.s().attachments.is_empty() {
            return;
        }
        // Recalled text cannot bring a pasted picture back, so history keeps the words only.
        let words = image::strip_tokens(&text);
        if !words.trim().is_empty() {
            let sid = self.sessions[self.active].session_id.clone();
            history::record_prompt(self.cwd(), &words, sid.as_deref());
            if self.history.last() != Some(&words) {
                self.history.push(words);
            }
        }
        // A time was picked with /schedule: this message waits for it.
        if let Some(due) = self.sched_at.take() {
            return self.queue_message(text, Some(due));
        }
        // `!command` runs here, as in Claude Code: no model turn, its output rides along with the next message.
        if let Some(cmd) = text.trim().strip_prefix('!').map(str::trim).filter(|c| !c.is_empty()) {
            let (cmd, sid, tx) = (cmd.to_string(), self.sessions[self.active].id, self.tx.clone());
            let dir = self.sessions[self.active].cwd.clone().unwrap_or_else(|| self.cwd.clone());
            let id = self.s().start_shell(&cmd);
            let login = std::env::var("SHELL").ok().filter(|_| self.shell_login);
            // ponytail: no timeout or cancel; a command that never ends just never reports (the UI stays free).
            std::thread::spawn(move || {
                let out = shell::command(login.as_deref(), &cmd).current_dir(dir).stdin(std::process::Stdio::null()).output();
                let _ = tx.send(AppEvent::Shell(sid, id, cmd, out));
            });
            return;
        }
        // `/swarm` sends one message that has Claude use its own Agent tool; from here on it is an ordinary message.
        let text = match text.trim().strip_prefix("/swarm").filter(|r| r.is_empty() || r.starts_with(' ')) {
            Some(rest) => match self.swarm_text(rest) {
                Ok(t) => t,
                Err(e) => return self.flash(e),
            },
            None => text,
        };
        let (cmd, arg) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
        let arg = arg.trim().to_string();
        match cmd {
            "/quit" | "/exit" => self.quit = true,
            // In the app only the keys and commands matter; the flags are for the command line.
            "/help" => self.s().note(HELP[HELP.find("KEYS").unwrap_or(0)..].trim_end().into()),
            "/new" => {
                if !arg.is_empty()
                    && let Err(e) = profile_args(&arg)
                {
                    return self.flash(e);
                }
                self.new_tab();
                if !arg.is_empty() {
                    self.s().profile = arg;
                }
            }
            "/profile" if arg.is_empty() => {
                let cur = self.s().profile.clone();
                let all = ["full", "sources", "lean"].map(String::from).into_iter().chain(profile::names()).collect::<Vec<_>>().join(" | ");
                let dir = profile::dir().map(|d| d.display().to_string()).unwrap_or_default();
                self.s().note(format!("profile: {cur}. Available: {all}. /profile <name> changes it for this tab; /new <name> opens a tab with one. Your own live in {dir}/<name>.json"));
            }
            "/profile" => {
                let (name, hard) = match arg.strip_suffix(" --hard") {
                    Some(n) => (n.trim().to_string(), true),
                    None => (arg.clone(), false),
                };
                if let Err(e) = profile_args(&name) {
                    return self.flash(e);
                }
                if self.s().profile == name && !hard {
                    return;
                }
                if let Err(e) = self.s().restart_later() {
                    return self.flash(e);
                }
                let started = self.s().ctx_tokens > 0;
                let k = self.s().ctx_tokens / 1000;
                self.s().profile = name.clone();
                let msg = if hard {
                    self.s().unpinned = true;
                    format!("profile → {name} with a re-rendered prompt; the next message re-caches up to ~{k}k tokens")
                } else if started {
                    format!(
                        "profile → {name}: tool/MCP changes apply on your next message. This conversation keeps the system prompt and settings it started with (Claude Code pins them); `/profile {name} --hard` re-renders them (re-caches up to ~{k}k tokens), `/new {name}` starts clean"
                    )
                } else {
                    format!("profile → {name}")
                };
                self.s().note(msg);
            }
            "/resume" if arg.is_empty() => self.open_explorer(),
            "/sessions" => self.toggle_sessions(),
            "/resume" => {
                // An id, or the start of one; the explorer is for when you do not know it.
                // Also a conversation's name, and from any folder, as `claude --resume` takes them.
                let hit = |f: &history::Summary| f.id.starts_with(&arg) || f.title.eq_ignore_ascii_case(&arg);
                let mut here: Vec<_> = history::recent(self.cwd(), 500).into_iter().filter(|f| hit(f)).map(|f| (f, None)).collect();
                if here.is_empty() {
                    here = history::recent_all(2000).into_iter().filter(|f| hit(f)).map(|f| { let d = Some(f.cwd.clone()).filter(|d| !d.is_empty()); (f, d) }).collect();
                }
                if here.len() > 1 && !here.iter().any(|(f, _)| f.id == arg) {
                    let n = here.len();
                    self.s().note(format!("{n} sessions match {arg:?}: type more of the id, or /resume to pick one"));
                    return;
                }
                let found = here.into_iter().next();
                match (found, history::find(&arg)) {
                    (Some((f, dir)), _) => self.open_session(f.id, f.title, dir),
                    (None, Some(_)) => self.open_session(arg, String::new(), None),
                    (None, None) => self.s().note(format!("no session with an id starting {arg:?} or named so; /resume lists them")),
                }
            }
            "/version" => self.version_cmd(&arg),
            "/cd" => self.cd_cmd(&arg),
            "/worktree" | "/wt" => self.worktree_cmd(&arg),
            "/term" => self.term_cmd(&arg),
            "/open" => {
                let found = open::collect(&self.sessions[self.active].items, std::path::Path::new(self.cwd()));
                if arg.is_empty() {
                    return self.open_ui = Some(open::Ui { sel: 0, found });
                }
                let hit = arg.parse::<usize>().ok().and_then(|n| found.get(n.wrapping_sub(1)).cloned()).or_else(|| open::parse_token(&arg, std::path::Path::new(self.cwd())));
                match hit {
                    Some(f) => self.open_found(&f),
                    None => self.flash(format!("nothing to open for {arg:?}: give a number from /open, an http(s) link, or a path that exists")),
                }
            }
            "/statusline" if arg == "items" => self.items_ui = Some(segments::Ui::default()),
            "/statusline" => match statusline::Mode::parse(&arg).filter(|m| !matches!(m, statusline::Mode::Command(_))) {
                None if arg.is_empty() => {
                    let claude = statusline::lookup(std::path::Path::new(self.cwd()), &self.claude_paths).found;
                    let theirs = claude.map_or("none found in your Claude Code settings".to_string(), |c| format!("`{}` from {:?} settings", c.command, c.source));
                    let now = match &self.statusline_mode {
                        statusline::Mode::Builtin => "builtin: claude++'s own line only, nothing is spawned".to_string(),
                        statusline::Mode::Claude => format!("claude: running {theirs}"),
                        statusline::Mode::Command(c) => format!("command (from --statusline, not saved): `{c}`"),
                    };
                    self.s().note(format!("status line → {now}\nClaude Code's would be: {theirs}\n/statusline claude | builtin switches and remembers the choice; /statusline items picks the parts of the builtin line"));
                }
                None => self.flash("usage: /statusline [claude|builtin|items]"),
                Some(mode) => {
                    let name = mode.name().to_string();
                    let note = self.apply_statusline_mode(mode);
                    let saved = match self.config_path.clone() {
                        Some(p) => match config::set_str(&p, "statusline", &name) {
                            Ok(()) => format!("remembered in {}", p.display()),
                            Err(e) => format!("could not save to {}: {e}", p.display()),
                        },
                        None => "not saved (no config location)".into(),
                    };
                    let extra = note.map(|n| format!(". {n}, so claude++'s own line is shown")).unwrap_or_default();
                    self.s().note(format!("status line → {name}{extra} ({saved})"));
                }
            },
            "/config" | "/setting" => self.settings = Some(settings::Ui::default()),
            "/session" => self.tabcfg = Some(tabcfg::Ui::default()),
            "/optimize" => self.optimize_cmd(&arg),
            "/permissions" => self.perms = Some(permissions::Ui::new(self.cwd(), &self.sessions[self.active])),
            "/instructions" => self.instr = Some(instructions::Ui::new(self.cwd(), instructions::rules_path().map(|f| instructions::load_rules(&f)).unwrap_or_default())),
            "/btw" => self.btw_cmd(&arg),
            "/usage" | "/cost" | "/status" => self.open_usage(),
            "/context" => {
                self.ctx_ui = Some(Default::default());
                self.ask_usage();
            }
            "/mode" if arg.is_empty() => self.tabcfg = Some(tabcfg::Ui::default()),
            "/memory" => {
                let dir = if arg == "user" { config::claude_dir() } else { Some(self.cwd().into()) };
                self.edit_file = dir.map(|d| d.join("CLAUDE.md"));
            }
            "/export" => self.export(&arg),
            "/tasks" => self.lanes = Some(Default::default()),
            "/procs" => self.open_procs(),
            "/supervisor" => self.open_supervisor(),
            "/queue" => self.queue_ui = Some(Default::default()),
            "/schedule" => {
                // `10m do this` schedules at once; `2h` waits for the message to be typed; anything else asks for the time.
                let arg = arg.trim();
                let (when, msg) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
                let (msg, due) = (msg.trim(), queue::parse_when(when, SystemTime::now()));
                match due {
                    Some(due) if !msg.is_empty() => self.queue_message(msg.to_string(), Some(due)),
                    Some(due) => {
                        self.sched_at = Some(due);
                        self.flash("now type the message: enter schedules it, esc cancels");
                    }
                    None => {
                        let mut ui = schedule::Ui::new(when);
                        ui.msg = Some(msg.to_string()).filter(|m| !m.is_empty());
                        self.sched_ui = Some(ui);
                    }
                }
            }
            "/trust" => {
                let cwd = std::path::PathBuf::from(self.cwd());
                trust::allow(&cwd);
                self.sessions.iter_mut().for_each(|s| s.trust = None);
                self.flash(format!("starting claude in {} although Claude Code has not trusted it", cwd.display()));
                self.warm_active();
            }
            "/search" => {
                self.open_search();
                if !arg.is_empty() && let Some(ui) = self.search.as_mut() {
                    ui.query = arg;
                    ui.refresh(&self.sessions[self.active].items, &self.history);
                    let at = ui.reveal();
                    self.s().reveal = at;
                }
            }
            "/editor" => {
                self.apply_setting("input_mode", if self.editor_mode { "default" } else { "editor" });
                self.flash(if self.editor_mode { "editor mode on: the message area is your editor, quit it to send; ctrl+\\ cancels; /editor turns it off" } else { "editor mode off" });
            }
            "/vim" => {
                self.apply_setting("input_mode", if self.vim.on { "default" } else { "vim" });
                self.flash(if self.vim.on { "vim keys on: esc for normal mode; /vim turns them off" } else { "vim keys off" });
            }
            "/close" => self.close_tab_asking(),
            "/clear" => self.s().clear(),
            "/copy" => self.copy_last(arg.as_str()),
            "/find" => {
                let s = self.s();
                let hit = match arg.as_str() {
                    "" if s.find_query.is_empty() => return self.flash("usage: /find <text>; /find again steps to the next older match, /find + to a newer one"),
                    "" => s.find_step(true),
                    "+" => s.find_step(false),
                    q => s.find(q),
                };
                let q = s.find_query.clone();
                match hit {
                    Some((k, n)) => self.flash(format!("{q:?}: match {k} of {n} (newest first); /find steps older, /find + newer")),
                    None => self.flash(format!("{q:?}: no match in this conversation")),
                }
            }
            "/img" if arg.is_empty() => self.flash("usage: /img <path to png/jpeg/gif/webp>"),
            "/img" => self.attach(arg.trim_matches(|c| c == '\'' || c == '"')),
            "/model" | "/effort" if arg.is_empty() => self.open_model_picker(),
            "/color" if arg.is_empty() => {
                let cur = self.sessions[self.active].color.as_deref().unwrap_or("default");
                self.color_picker = Self::color_choices().iter().position(|c| *c == cur);
            }
            "/model" => {
                let warn = self.s().recache_warning();
                self.s().choose_model(&arg);
                self.s().note(format!("model → {arg}.{warn}"));
            }
            "/effort" => {
                let known = self.model_list().iter().find(|m| self.sessions[self.active].model == m.resolved).map(|m| m.efforts.clone());
                match known.filter(|e| !e.is_empty()) {
                    Some(levels) if !levels.contains(&arg) => self.s().note(format!("effort: this model supports {}", levels.join(", "))),
                    _ => {
                        self.s().choose_effort(&arg);
                        self.s().note(format!("effort → {arg}"));
                    }
                }
            }
            "/mode" if !arg.is_empty() => {
                self.s().control("set_permission_mode", json!({"mode": arg}));
                self.s().mode = arg;
                self.s().status_dirty = true;
            }
            _ => {
                if cmd == "/color" {
                    self.s().set_color(&arg);
                }
                let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
                let mentioned = self.mentioned_images(&text);
                if !text.starts_with('/')
                    && let Some(w) = self.limit_block()
                {
                    return self.hold_for_limit(text, w);
                }
                // Claude is working (or messages already wait): this one waits its turn, managed with alt+q.
                if !text.starts_with('/') && ((self.queue_on && self.s().busy) || self.s().queue.iter().any(|q| q.due.is_none())) {
                    self.queue_message(text, None);
                    return self.drain_queues();
                }
                self.undo.clear(); // sent: what was only kept for undo goes now
                self.redo.clear();
                self.drop_orphan_images(&text);
                self.s().submit(text, &mentioned, &launch, &tx);
            }
        }
    }
    /// `/version`: the installed Claude Code versions, or pin one (`/version 2.1.317`) or unpin (`/version default`).
    fn version_cmd(&mut self, arg: &str) {
        let list = self.versions_dir.as_deref().map(version::installed_in).unwrap_or_default();
        let default = self.versions_dir.as_deref().and_then(|d| version::default_in(d, &self.launch.bin));
        let arg = arg.trim();
        if arg.is_empty() {
            let running: Vec<(String, String)> = self.sessions.iter().enumerate().filter(|(_, s)| s.backend.is_some() && !s.claude_version().is_empty()).map(|(i, s)| (format!("tab {}", i + 1), s.claude_version().to_string())).collect();
            let text = version::describe(&list, default.as_deref(), self.pin.as_ref().map(|p| p.version.as_str()), session::VERIFIED_LINE, &running);
            return self.s().note(text);
        }
        let target = match arg {
            "default" | "unpin" => None,
            v => match list.iter().find(|i| i.version == v) {
                Some(i) => Some(i.clone()),
                None => return self.flash(format!("Claude Code {v} is not installed here; /version lists what is (`claude install {v}` fetches it)")),
            },
        };
        if target.is_some() && self.launch.bin != "claude" {
            return self.flash("--claude-bin names the executable for this run, so there is nothing to pin");
        }
        let want = target.as_ref().map(|t| t.version.clone()).or(default);
        self.pin = target;
        if let Some(p) = &self.config_path {
            let _ = config::set_str(p, "claude_version", self.pin.as_ref().map_or("", |t| t.version.as_str()));
        }
        // Tabs that are idle move over at once (their next message resumes on the other version); a busy one is left alone.
        let (mut moved, mut stay) = (0, 0);
        for s in &mut self.sessions {
            if s.backend.is_some() && Some(s.claude_version()) != want.as_deref() {
                if s.restart_later().is_ok() { moved += 1 } else { stay += 1 }
            }
        }
        let now = match &self.pin {
            Some(p) => format!("Claude Code pinned to {} (its updater is off for claude++'s tabs)", p.version),
            None => "Claude Code unpinned: tabs run the `claude` on your PATH again".into(),
        };
        let tabs = match (moved, stay) {
            (0, 0) => String::new(),
            (m, 0) => format!("; {m} idle tab(s) switch on their next message"),
            (m, s) => format!("; {m} idle tab(s) switch on their next message, {s} busy one(s) keep their version until they are restarted"),
        };
        self.s().note(format!("{now}{tabs}"));
    }
    /// `/cd <path>`: this tab works in another folder from its next message. The conversation goes with it: Claude
    /// Code resumes it from the new folder (tools run there, its CLAUDE.md and settings load), and the transcript stays
    /// where it was. `/cd` alone says where the tab is, `/cd -` goes back, `~` and relative paths work.
    pub(crate) fn cd_cmd(&mut self, arg: &str) {
        let here = self.cwd().to_string();
        let arg = arg.trim().trim_matches(['"', '\'']);
        if arg.is_empty() {
            return self.s().note(format!("working folder: {}  (/cd <path> changes it, /cd - goes back)", ui::tilde(&here)));
        }
        let want = if arg == "-" {
            match self.sessions[self.active].prev_cwd.clone() {
                Some(p) => p,
                None => return self.flash("no earlier folder in this tab"),
            }
        } else if arg == "~" || arg.starts_with("~/") {
            match std::env::home_dir() {
                Some(h) => h.join(arg.trim_start_matches('~').trim_start_matches('/')).to_string_lossy().into_owned(),
                None => return self.flash("no home folder to resolve ~"),
            }
        } else {
            std::path::Path::new(&here).join(arg).to_string_lossy().into_owned()
        };
        let Ok(dir) = std::fs::canonicalize(&want).and_then(|p| if p.is_dir() { Ok(p) } else { Err(std::io::ErrorKind::NotADirectory.into()) }) else {
            return self.flash(format!("not a folder: {}", ui::tilde(&want)));
        };
        let dir = dir.to_string_lossy().into_owned();
        if dir == here {
            return self.flash("already there");
        }
        if let Err(e) = self.s().restart_later() {
            return self.flash(e);
        }
        if !self.profile_forced && let Some(p) = project_profile(&dir) {
            self.s().profile = p;
        }
        let started = self.s().ctx_tokens > 0;
        let s = self.s();
        s.prev_cwd = Some(here);
        s.cwd = Some(dir.clone());
        self.files_at = None;
        let more = if started { "; the conversation continues from there on your next message (the prompt cache is normally kept)" } else { "" };
        self.s().note(format!("working folder → {}{more}", ui::tilde(&dir)));
    }
    /// `/worktree`: the panel; `/worktree add <name> [base]` and `/worktree rm <name|path>` do it without it.
    fn worktree_cmd(&mut self, arg: &str) {
        let cwd = self.cwd().to_string();
        let (verb, rest) = arg.split_once(' ').map_or((arg, ""), |(v, r)| (v, r.trim()));
        match verb {
            "" | "list" => match worktree::list(&cwd) {
                Ok(list) => self.wt_ui = Some(worktree::Ui { list, ..Default::default() }),
                Err(e) => self.flash(e),
            },
            "add" | "new" => {
                let (name, base) = rest.split_once(' ').map_or((rest, None), |(n, b)| (n, Some(b.trim().to_string())));
                self.wt_add(name, base);
            }
            "rm" | "remove" => match worktree::list(&cwd) {
                Ok(list) => match list.iter().skip(1).find(|w| w.path == rest || w.branch.as_deref() == Some(rest) || w.path.rsplit('/').next() == Some(rest)) {
                    Some(w) => self.wt_remove(&w.path.clone()),
                    None => self.flash(format!("no worktree {rest:?} besides the main one; /worktree lists them")),
                },
                Err(e) => self.flash(e),
            },
            _ => self.flash("usage: /worktree [add <name> [base]] [rm <name|path>]"),
        }
    }

    /// `git worktree add` off the UI thread (a checkout can take a while); the new folder opens in a tab when it is done.
    pub(crate) fn wt_add(&mut self, name: &str, base: Option<String>) {
        if !worktree::valid_name(name) {
            return self.flash(format!("{name:?} is not a usable name: letters, digits and . _ - / only"));
        }
        let (cwd, name, tx) = (self.cwd().to_string(), name.to_string(), self.tx.clone());
        self.flash(format!("adding worktree {name}…"));
        std::thread::spawn(move || {
            let res = worktree::add(&cwd, &name, base.as_deref()).map(|p| (format!("worktree {name} added: {}", ui::tilde(&p)), Some(p)));
            let _ = tx.send(AppEvent::Worktree(res));
        });
    }

    pub(crate) fn wt_remove(&mut self, path: &str) {
        let (cwd, path, tx) = (self.cwd().to_string(), path.to_string(), self.tx.clone());
        if self.sessions.iter().any(|s| s.cwd.as_deref().unwrap_or(&self.cwd) == path) {
            return self.flash("a tab is working in it: close or /cd that tab first");
        }
        std::thread::spawn(move || {
            let res = worktree::remove(&cwd, &path).map(|()| (format!("worktree removed: {}", ui::tilde(&path)), None));
            let _ = tx.send(AppEvent::Worktree(res));
        });
    }
    /// The message `/swarm` sends. Refused where Claude has no `Agent` tool (the `lean` profile).
    fn swarm_text(&mut self, arg: &str) -> Result<String, String> {
        let (n, goal) = swarm::parse(arg)?;
        let s = &self.sessions[self.active];
        let has_agent = if s.tools.is_empty() { s.profile != "lean" } else { s.tools.iter().any(|t| t == "Agent" || t == "Task") };
        if !has_agent {
            return Err("this tab has no Agent tool (the lean profile leaves it out): /profile full, or a profile with Agent added".into());
        }
        let msg = format!("swarm: up to {n} subagents inside this one claude process (about 4 MB each); every agent bills its own tokens, so expect up to ~{n}x the usage. /tasks shows them working.");
        self.s().note(msg);
        Ok(swarm::prompt(n, &goal))
    }
}

impl App {
    /// Tell the user something happened in a tab they are not looking at: a bell, or a desktop notification
    /// (OSC 9: iTerm2, Ghostty, WezTerm...), as configured.
    pub(crate) fn notify(&mut self, what: &str) {
        let seq = match self.notify.as_str() {
            "bell" => "\x07".to_string(),
            "desktop" => format!("\x1b]9;claude++: {}\x07", what.replace(['\x07', '\x1b'], "")),
            _ => return,
        };
        #[cfg(test)]
        self.sent_notices.push(seq.clone());
        if self.tty {
            let _ = write!(io::stdout(), "{seq}");
            let _ = io::stdout().flush();
        }
    }

    /// `/copy`: the last answer, or with `code` the last fenced code block in it.
    /// `/export [file]`: the conversation as plain text, your messages marked `>` and tool calls `●`.
    pub(crate) fn export(&mut self, arg: &str) {
        let mut out = String::new();
        for i in &self.sessions[self.active].items {
            let block = match &i.entry {
                Entry::User(_) | Entry::Rich(_) => i.entry.text().lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n"),
                Entry::Assistant { text, .. } => text.trim_end().to_string(),
                Entry::Tool { name, input, result, nested, .. } if !nested && name != "Thinking" => {
                    let res = result.as_ref().map_or(String::new(), |r| {
                        let mut t: String = r.0.trim_end().chars().take(2000).collect();
                        if t.len() < r.0.trim_end().len() {
                            t.push_str("\n…");
                        }
                        t.lines().map(|l| format!("\n  ⎿ {l}")).collect()
                    });
                    let mut input = input.clone();
                    if let Some(o) = input.as_object_mut() {
                        o.retain(|k, _| !k.starts_with('_')); // our own bookkeeping (patches, agent steps)
                    }
                    format!("● {name}({input}){res}")
                }
                _ => continue,
            };
            out.push_str(&block);
            out.push_str("\n\n");
        }
        if out.is_empty() {
            return self.flash("nothing to export yet");
        }
        let name = if arg.is_empty() {
            let stamp = std::process::Command::new("date").arg("+%Y-%m-%d-%H%M%S").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            format!("conversation-{stamp}.txt")
        } else {
            arg.to_string()
        };
        let path = std::path::Path::new(self.cwd()).join(name);
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path);
        match file.and_then(|mut f| f.write_all(out.as_bytes())) {
            Ok(()) => self.flash(format!("exported to {}", path.display())),
            Err(e) => self.flash(format!("export failed: {e}")),
        }
    }

    pub(crate) fn copy_last(&mut self, what: &str) {
        let last = self.sessions[self.active].items.iter().rev().find_map(|i| match &i.entry {
            Entry::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        });
        let Some(text) = last else { return self.flash("nothing to copy yet") };
        let (text, label) = match what {
            "" => (text, "answer"),
            "code" => match last_code_block(&text) {
                Some(code) => (code, "code block"),
                None => return self.flash("the last answer has no code block"),
            },
            _ => return self.flash("usage: /copy [code]"),
        };
        let n = text.chars().count();
        self.copy_text(&text);
        self.flash(format!("copied the last {label} ({n} chars)"));
    }

    /// Clipboard via the OSC 52 escape (works over ssh in terminals that allow it) and, where it exists,
    /// `pbcopy`.
    pub(crate) fn copy_text(&mut self, text: &str) {
        #[cfg(test)]
        self.sent_notices.push(format!("copy:{text}"));
        if self.tty {
            let _ = write!(io::stdout(), "\x1b]52;c;{}\x07", image::base64(text.as_bytes()));
            let _ = io::stdout().flush();
        }
        if cfg!(target_os = "macos") && self.tty
            && let Ok(mut c) = std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn()
        {
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(text.as_bytes());
            }
            thread::spawn(move || drop(c.wait()));
        }
    }

    /// Put a message in this tab's queue, with the pictures it mentions.
    pub(crate) fn queue_message(&mut self, text: String, due: Option<SystemTime>) {
        let used = image::tokens_in(&text);
        let (keep, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.s().attachments).into_iter().partition(|a| used.contains(&a.n));
        gone.iter().for_each(|a| a.preview.iter().for_each(|p| image::delete(p.id)));
        let keep = keep
            .into_iter()
            .map(|mut a| {
                a.preview.take().iter().for_each(|p| image::delete(p.id)); // the box no longer shows it
                a
            })
            .collect();
        self.undo.clear();
        self.redo.clear();
        let s = self.s();
        if due.is_none() {
            s.queue_hold = false; // sending something new means carrying on
        }
        s.queue.push(session::Queued { text, atts: keep, due });
        let n = s.queue.len();
        self.flash(match due {
            Some(d) => format!("scheduled for {} · alt+q to manage", queue::clock(d)),
            None => format!("queued ({n}) · alt+q to manage"),
        });
    }

    /// A tab whose prompt cache is about to expire and whose conversation is big gets `/compact` now, while
    /// the cache still serves the read: the next message then re-caches a summary, not the whole conversation.
    pub(crate) fn precompact(&mut self) {
        if self.precompact_k == 0 {
            return;
        }
        let (min, ttl, typing) = (self.precompact_k * 1000, self.cache_ttl, !self.input.is_empty());
        for i in 0..self.sessions.len() {
            let s = &mut self.sessions[i];
            if !s.should_precompact(ttl, min) || (i == self.active && typing) {
                continue;
            }
            s.precompacted = true;
            let left = s.cache_left(ttl).map_or(0, |l| l.as_secs());
            s.note(format!("{}k tokens and the prompt cache expires in {left}s: compacting now, while it is still warm, so your next message re-caches a summary instead", s.ctx_tokens / 1000));
            self.send_queued(i, session::Queued { text: "/compact".into(), atts: vec![], due: None });
        }
    }

    /// Send the next waiting message of every tab that is free to take one.
    pub(crate) fn drain_queues(&mut self) {
        let held = self.limit_block();
        for i in 0..self.sessions.len() {
            let s = &mut self.sessions[i];
            if let Some(w) = held {
                // Messages that were waiting for the turn now wait for the window instead.
                s.queue.iter_mut().filter(|q| q.due.is_none()).for_each(|q| q.due = Some(w.resets_at()));
            }
            if s.busy || s.queue_hold || s.rewinding || !s.pending.is_empty() {
                continue;
            }
            // The first message that is not waiting for a time of its own.
            let now = SystemTime::now();
            let Some(at) = s.queue.iter().position(|q| q.due.is_none_or(|d| d <= now)) else { continue };
            let q = s.queue.remove(at);
            self.send_queued(i, q);
        }
    }

    pub(crate) fn send_queued(&mut self, i: usize, q: session::Queued) {
        let (launch, tx) = (self.launch_for(i), self.tx.clone());
        let mentioned = self.mentioned_images(&q.text);
        let mine = std::mem::replace(&mut self.sessions[i].attachments, q.atts);
        self.sessions[i].submit(q.text, &mentioned, &launch, &tx);
        self.sessions[i].attachments = mine;
    }

    pub(crate) fn queue_key(&mut self, k: KeyEvent) {
        use queue::Act;
        let Some(mut ui) = self.queue_ui.take() else { return };
        let i = self.active;
        let act = ui.key(k, &mut self.sessions[i].queue);
        let keep = match act {
            Act::Close => false,
            Act::None => true,
            Act::Send(n) => {
                // It goes first; if Claude is mid-turn that turn is stopped, and the message follows its end.
                let s = &mut self.sessions[i];
                let mut q = s.queue.remove(n);
                q.due = None;
                s.queue.insert(0, q);
                self.release_limit();
                let s = &mut self.sessions[i];
                s.interrupt();
                s.queue_hold = false;
                self.drain_queues();
                true
            }
            Act::Steer(n) => {
                let mut q = self.sessions[i].queue.remove(n);
                q.due = None;
                self.release_limit();
                self.send_queued(i, q);
                true
            }
            Act::Edit(_) if !self.input.is_empty() => {
                self.flash("send or clear the message box first: editing a queued message needs it");
                true
            }
            Act::Edit(n) => {
                let q = self.sessions[i].queue.remove(n);
                self.set_input(q.text);
                self.sessions[i].attachments.extend(q.atts);
                false
            }
            Act::Remove(n) => {
                self.sessions[i].queue.remove(n);
                true
            }
            Act::Clear => {
                self.sessions[i].queue.clear();
                true
            }
            Act::Schedule(n) => {
                let mut s = schedule::Ui::new("");
                (s.target, s.from_queue) = (Some(n), true);
                self.sched_ui = Some(s);
                false
            }
            Act::Pause => {
                let s = &mut self.sessions[i];
                s.queue_hold = !s.queue_hold;
                self.drain_queues();
                true
            }
        };
        if keep {
            self.queue_ui = Some(ui);
        }
    }

    pub(crate) fn schedule_key(&mut self, k: KeyEvent) {
        use schedule::Act;
        let Some(mut ui) = self.sched_ui.take() else { return };
        let i = self.active;
        let unset = ui.target.and_then(|n| self.sessions[i].queue.get(n)).is_some_and(|q| q.due.is_some());
        let done = match ui.key(k, SystemTime::now(), unset) {
            Act::None => false,
            Act::Close => true,
            Act::Bad => {
                self.flash("not a time: try 45m, 2h30m or 17:30");
                false
            }
            Act::Pick(due) => {
                if let Some(n) = ui.target {
                    self.sessions[i].queue.get_mut(n).iter_mut().for_each(|q| q.due = due);
                    self.flash(due.map_or("no longer scheduled: it goes when the turn ends".into(), |d| format!("scheduled for {}", queue::clock(d))));
                } else if let (Some(msg), Some(due)) = (ui.msg.take(), due) {
                    self.queue_message(msg, Some(due));
                } else {
                    self.sched_at = due;
                    self.flash("now type the message: enter schedules it, esc cancels");
                }
                true
            }
        };
        if !done {
            self.sched_ui = Some(ui);
        } else if ui.from_queue {
            self.queue_ui = Some(Default::default());
        }
    }
}

/// The body of the last ``` fenced block in `text`.
pub(crate) fn last_code_block(text: &str) -> Option<String> {
    let mut blocks = vec![];
    let mut cur: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match cur.take() {
                Some(b) => blocks.push(b.join("\n")),
                None => cur = Some(vec![]),
            }
        } else if let Some(b) = cur.as_mut() {
            b.push(line);
        }
    }
    blocks.pop()
}

/// Commands claude++ handles itself: (name, argument hint, description).
pub(crate) const LOCAL_COMMANDS: [(&str, &str, &str); 42] = [
    ("new", "[profile]", "open a new tab"),
    ("profile", "[name] [--hard]", "show or change this tab's profile"),
    ("close", "", "close this tab"),
    ("clear", "", "start a fresh conversation in this tab"),
    ("resume", "[id]", "browse and reopen earlier sessions"),
    ("sessions", "", "list sessions: this directory's and background ones (also ←)"),
    ("model", "[name]", "pick a model and effort, or switch directly"),
    ("effort", "[level]", "pick or set the thinking effort"),
    ("color", "[name]", "pick this conversation's colour: the tab, frames and highlights wear it"),
    ("mode", "[mode]", "set the permission mode (no argument: this tab's mode and rules)"),
    ("memory", "[user]", "edit this project's CLAUDE.md (or your own ~/.claude/CLAUDE.md) in $EDITOR"),
    ("export", "[file]", "save this conversation as text (default: a dated file here)"),
    ("img", "<path>", "attach an image to the next message"),
    ("find", "<text>", "jump to the newest match in the conversation; again for older"),
    ("queue", "", "messages waiting for Claude: reorder, send now, edit, delete (alt+q)"),
    ("schedule", "<when> <message>", "send a message later: 10m, 2h30m or a time like 14:30"),
    ("search", "[text]", "search this conversation or the messages you sent (ctrl+f)"),
    ("vim", "", "turn vim keys in the message box on or off"),
    ("editor", "", "turn the message box into your $EDITOR on or off (quit the editor to send)"),
    ("copy", "[code]", "copy the last answer, or its last code block, to the clipboard"),
    ("config", "", "claude++'s settings"),
    ("trust", "", "start claude in this folder although Claude Code has not trusted it (this run only)"),
    ("setting", "", "same as /config"),
    ("session", "", "this tab's permission mode and tool rules"),
    ("usage", "", "where this tab's context goes, and your plan limits"),
    ("context", "", "the context window drawn: what fills it, category by category"),
    ("tasks", "", "this tab's subagents: state, steps and report"),
    ("supervisor", "", "see what the claude++ supervisor is doing, change its settings, restart or stop it"),
    ("procs", "", "Claude processes on this machine: who owns each, what it holds (read-only)"),
    ("optimize", "[light|balanced|aggressive|off|now]", "trim this project's start-up context from evidence (reversible)"),
    ("permissions", "", "saved allow/ask/deny rules from your settings files: add and delete"),
    ("instructions", "", "tick CLAUDE.md files and sections in or out of new sessions"),
    ("term", "[command]", "a terminal in claude++ (a shell, or the command); Claude can drive terminals too: /config → Agent terminal"),
    ("btw", "<question>", "ask a side question about this conversation; it is not added to it"),
    ("swarm", "[2-8] <goal>", "have Claude split a goal into parallel subagents, each in its own worktree"),
    ("worktree", "[add <name> | rm <name>]", "list, add and remove git worktrees, each can have its own tab (also /wt)"),
    ("open", "[n|link|path]", "open a link or file from this conversation"),
    ("cd", "[path|-]", "work in another folder from the next message on (this conversation goes with you)"),
    ("version", "[x.y.z|default]", "list the installed Claude Code versions, or pin the one claude++ runs"),
    ("statusline", "[claude|builtin|items]", "choose which status line to show, or which parts of claude++'s own"),
    ("help", "", "keys and commands"),
    ("quit", "", "leave claude++"),
];

/// One row of the `/command` and `@file` popup.
#[derive(Clone)]
pub struct Completion {
    /// `/` for commands, `@` for files.
    pub prefix: char,
    pub name: String,
    pub hint: String,
    pub desc: String,
}
