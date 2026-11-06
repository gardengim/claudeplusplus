//! Sending: `/` commands claude++ handles itself, `!` shell commands, and the hand-off to the session.

use super::*;

impl App {
    pub(crate) fn submit(&mut self) {
        if self.sessions[self.active].rewinding {
            return self.flash("restoring files; send once they are back");
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
            // ponytail: no timeout or cancel; a command that never ends just never reports (the UI stays free).
            std::thread::spawn(move || {
                let out = std::process::Command::new("sh").args(["-c", &cmd]).current_dir(dir).stdin(std::process::Stdio::null()).output();
                let _ = tx.send(AppEvent::Shell(sid, id, cmd, out));
            });
            return;
        }
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
}
