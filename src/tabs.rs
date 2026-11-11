//! Tabs, the session list and the dock beside the conversation.

use super::*;

impl App {
    /// Start (or resume) the active tab's backend while the user is still typing, so its
    /// ~0.5 s startup overlaps with composing the message. Nothing is spawned for an idle UI,
    /// slash commands (once their list is known), or an empty input, and no API call is made until a
    /// message is sent.
    pub(crate) fn warm(&mut self) {
        // Typing `/` only starts a backend when completion still has no command list to offer.
        let wanted = !self.input.is_empty() && (!self.input.starts_with('/') || self.known_commands.is_empty());
        if wanted && self.sessions[self.active].backend.is_none() {
            let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
            self.s().warm(&launch, &tx);
        }
    }

    /// The terminal's title follows the active conversation (`claude++ · name`), written only when it changes.
    pub(crate) fn sync_title(&mut self) {
        let t = match self.sessions[self.active].title.as_str() {
            "" => "claude++".to_string(),
            n => format!("claude++ · {n}"),
        };
        if t != self.term_title {
            let first = self.term_title.is_empty();
            // Save the terminal's own title once so exit can restore it.
            let _ = write!(io::stdout(), "{}\x1b]2;{}\x07", if first { "\x1b[22;0t" } else { "" }, t.replace(|c: char| c.is_control(), ""));
            self.term_title = t;
        }
    }

    /// Launch settings for one tab: its profile's flags, then the global passthrough flags.
    pub(crate) fn launch_for(&self, i: usize) -> Launch {
        let mut l = self.launch.clone();
        if let Some(p) = &self.pin {
            l.bin = p.path.to_string_lossy().into_owned();
            l.env.push(("DISABLE_AUTOUPDATER".into(), "1".into())); // a pinned version stays put
        }
        let spec = profile::spec(&self.sessions[i].profile).unwrap_or_default();
        l.env.extend(spec.env);
        let mut args = spec.args;
        if self.stable_prompt {
            args.push("--exclude-dynamic-system-prompt-sections".to_string());
        }
        if self.autocompact_k > 0 {
            args.extend(["--autocompact".to_string(), format!("{}k", self.autocompact_k)]);
        }
        if self.sessions[i].unpinned {
            args.extend(["--system-prompt-snapshot".to_string(), "off".to_string()]);
        }
        args.append(&mut l.args);
        let mut settings = serde_json::Map::new();
        let cwd = self.sessions[i].cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let saved = optimize::load(&cwd);
        // Sharing hands the folder's stdio servers to claude++, so `claude` loads only what is left in its own
        // config. Not when a profile or `/optimize` already chose the MCP servers (lean has none).
        let chose_mcp = args.iter().any(|a| a == "--strict-mcp-config" || a == "--mcp-config") || saved.as_ref().is_some_and(|o| o.strict_mcp);
        if self.share_mcp && !chose_mcp && let Some(sh) = mcp::sharing(&cwd, saved.as_ref().and_then(|o| o.mcp_keep.as_deref())) {
            args.push("--strict-mcp-config".into());
            if let Some(f) = sh.rest {
                args.extend(["--mcp-config".into(), f.to_string_lossy().into_owned()]);
            }
            l.shared = sh.shared;
        }
        if let Some(o) = &saved {
            o.apply(&mut settings, &mut args, &cwd);
        }
        if let (Some(f), Some(cache)) = (instructions::rules_path(), config::cache_dir()) {
            let rules = instructions::merge(&instructions::load_rules(&f), &saved.map(|o| o.instructions).unwrap_or_default());
            instructions::apply(&rules, &cache, &mut args, &mut settings);
        }
        if !settings.is_empty() {
            args.extend(["--settings".into(), serde_json::Value::Object(settings).to_string()]);
        }
        l.args = args;
        l.cwd = self.sessions[i].cwd.clone();
        if self.agent_term {
            self.sessions[i].terms.set_cwd(&cwd);
            l.terms = Some(self.sessions[i].terms.clone());
        }
        l
    }

    pub(crate) fn new_tab(&mut self) {
        self.next_id += 1;
        let title = if self.sessions.is_empty() {
            std::path::Path::new(&self.cwd).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            String::new()
        };
        let mut s = Session::new(self.next_id, title);
        s.profile = self.default_profile.clone();
        if !self.profile_forced && let Some(p) = project_profile(&self.cwd) {
            s.profile = p;
        }
        s.cache_ttl = self.cache_ttl;
        let (tx, sid) = (self.tx.clone(), self.next_id);
        s.terms = term::Terms::new(std::sync::Arc::new(move || drop(tx.send(AppEvent::TermOut(sid)))));
        // Shown until claude reports the real mode, so a new tab does not claim "manual" by mistake.
        let flag = self.launch.args.iter().position(|a| a == "--permission-mode").and_then(|i| self.launch.args.get(i + 1));
        s.mode = flag.cloned().unwrap_or_else(|| permissions::default_mode(&self.cwd));
        self.leave_tab();
        self.sessions.push(s);
        self.active = self.sessions.len() - 1;
        self.load_draft();
    }

    /// Called just before another tab takes the front. Leaving counts as use, so its idle time restarts here
    /// (otherwise a tab idle for minutes is stopped the moment you look away); its `/tasks` list is its own.
    pub(crate) fn leave_tab(&mut self) {
        if let Some(s) = self.sessions.get_mut(self.active) {
            s.last_active = Instant::now();
        }
        self.lanes = None;
        self.ctx_ui = None;
        self.queue_ui = None;
        self.sched_ui = None;
        self.sched_at = None;
        self.search = None;
        self.save_draft();
    }

    /// The message box belongs to the tab in front: leaving a tab keeps what was typed in it.
    pub(crate) fn save_draft(&mut self) {
        self.reset_undo = true;
        if let Some(s) = self.sessions.get_mut(self.active) {
            s.draft = (std::mem::take(&mut self.input), self.cursor);
        }
    }

    pub(crate) fn load_draft(&mut self) {
        let s = &mut self.sessions[self.active];
        (self.input, self.cursor) = std::mem::take(&mut s.draft);
        self.cursor = self.cursor.min(self.input.chars().count());
    }

    pub(crate) fn goto(&mut self, i: usize) {
        if i != self.active {
            self.leave_tab();
            self.active = i;
            self.load_draft();
            self.files_at = None; // the `@` index belongs to the tab's directory
        }
    }

    /// Closing a tab that is mid-turn throws the turn away: ask once, by pressing the same thing again.
    pub(crate) fn close_tab_asking(&mut self) {
        let s = &self.sessions[self.active];
        let busy = s.busy || !s.pending.is_empty();
        if busy && !self.close_armed.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            self.close_armed = Some(Instant::now());
            return self.flash("this tab is still working: close it again to stop the turn and close");
        }
        self.close_armed = None;
        self.close_tab();
    }

    pub(crate) fn close_tab(&mut self) {
        self.sessions.remove(self.active);
        if self.sessions.is_empty() {
            self.quit = true;
            self.new_tab(); // keep `active` valid until the loop exits
        }
        self.active = self.active.min(self.sessions.len() - 1);
        self.load_draft(); // the closed tab's own draft goes with it
    }

    pub(crate) fn switch(&mut self, delta: isize) {
        let n = self.sessions.len() as isize;
        self.goto(((self.active as isize + delta).rem_euclid(n)) as usize);
    }

    /// The tab working in `path` comes to the front, or a new tab opens there.
    pub(crate) fn open_folder_tab(&mut self, path: &str) {
        let canon = |p: &str| std::fs::canonicalize(p).map_or(p.to_string(), |c| c.to_string_lossy().into_owned());
        let want = canon(path);
        if let Some(i) = self.sessions.iter().position(|s| canon(s.cwd.as_deref().unwrap_or(&self.cwd)) == want) {
            return self.goto(i);
        }
        self.new_tab();
        if !self.profile_forced && let Some(p) = project_profile(&want) {
            self.s().profile = p;
        }
        let mode = permissions::default_mode(&want);
        let s = self.s();
        s.title = std::path::Path::new(&want).file_name().map(|n| n.to_string_lossy().chars().take(18).collect()).unwrap_or_default();
        if !self.launch.args.iter().any(|a| a == "--permission-mode") {
            self.s().mode = mode;
        }
        self.s().cwd = Some(want);
        self.files_at = None;
    }

    /// Re-read the open `/worktree` panel's list (after an add or remove).
    pub(crate) fn wt_refresh(&mut self) {
        let cwd = self.cwd().to_string();
        if let Some(ui) = self.wt_ui.as_mut() {
            if let Ok(l) = worktree::list(&cwd) {
                ui.sel = ui.sel.min(l.len().saturating_sub(1));
                ui.list = l;
            }
            ui.armed = None;
        }
    }

    pub(crate) fn open_found(&mut self, f: &open::Found) {
        let (args, said) = open::plan(f);
        match open::run(args) {
            Ok(()) => self.flash(said),
            Err(e) => self.flash(format!("could not open it: {e}")),
        }
    }

    #[cfg(test)]
    pub(crate) fn profile_flag_error(&self, p: &str) -> bool {
        profile_args(p).is_err()
    }

    /// Every open tab that has a stored conversation, with what it is doing now.
    pub fn open_states(&self) -> Vec<(String, jobs::State)> {
        self.sessions
            .iter()
            .filter_map(|s| {
                let st = if !s.pending.is_empty() { jobs::State::Blocked } else if s.busy { jobs::State::Working } else { jobs::State::Done };
                s.conversation().map(|id| (id, st))
            })
            .collect()
    }

    /// The side panel follows the cursor: an open conversation is switched to at once; a stored one is
    /// previewed in the main area (`dock_peek`) until Enter opens it in a new tab.
    pub(crate) fn dock_follow(&mut self) {
        let Some((id, tab)) = self.dock.as_ref().and_then(|d| d.selected()) else {
            self.dock_peek = false; // on a heading: nothing to follow
            return;
        };
        self.dock_peek = !tab;
        if let Some(i) = self.sessions.iter().position(|s| s.conversation().as_deref() == Some(id.as_str())) {
            self.goto(i);
        }
    }

    /// The side panel is either focused or gone: there is no docked-but-idle state (for now).
    pub(crate) fn hide_dock(&mut self) {
        (self.dock, self.dock_focus, self.dock_peek) = (None, false, false);
    }

    pub(crate) fn show_dock(&mut self, focus: bool) {
        if self.dock.is_none() {
            let open = self.open_states();
            let mut d = explorer::Explorer::from_cache(self.cwd(), open, self.list_cache.as_ref());
            self.load_sessions();
            d.set_folds(self.list_prefs.1.clone());
            if let Some(id) = self.active_id() {
                d.select(&id);
            }
            self.dock = Some(d);
            self.dock_at = Instant::now();
        }
        if focus && !self.dock_focus && let (Some(id), Some(d)) = (self.active_id(), self.dock.as_mut()) {
            d.select(&id); // taking the keys starts on the conversation in view, not wherever the cursor was left
        }
        self.dock_focus = focus;
    }

    /// /sessions: the docked list when configured (toggle), else the full-screen one.
    pub(crate) fn toggle_sessions(&mut self) {
        if !self.dock_mode {
            self.open_explorer();
        } else if self.dock.take().is_some() {
            self.dock_focus = false;
        } else {
            self.show_dock(true);
        }
    }

    pub(crate) fn dock_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Left => self.open_explorer(),
            KeyCode::Esc if !self.dock.as_ref().is_some_and(|d| d.filtering()) => self.hide_dock(),
            _ => {
                if let Some(o) = self.dock.as_mut().map(|d| d.key(k)) {
                    // → is "done browsing": the conversation in view stays, the panel goes.
                    let leave = matches!(o, explorer::Outcome::OpenHere(..) | explorer::Outcome::Close);
                    let opened = matches!(o, explorer::Outcome::Open(..));
                    self.explorer_outcome(o);
                    if leave {
                        (self.dock, self.dock_focus, self.dock_peek) = (None, false, false);
                    } else if opened {
                        self.hide_dock(); // typing now goes to that conversation
                    } else {
                        self.dock_follow();
                    }
                }
            }
        }
    }

    /// What a list row asked for (open, fork, stop...), from the full-screen list or the dock.
    pub(crate) fn explorer_outcome(&mut self, o: explorer::Outcome) {
        use explorer::Outcome::*;
        match o {
            Open(id, title, cwd) => self.open_session(id, title, cwd),
            OpenHere(id, title, cwd) => self.open_here(id, title, cwd),
            Fork(id, title, cwd) => {
                self.open_in_tab(id, title, cwd);
                self.s().fork = true;
                self.s().note("forked: this is a copy; the original is untouched".into());
            }
            Stop(short, then) => {
                self.flash(format!("stopping {short}…"));
                let (bin, tx) = (self.launch.bin.clone(), self.tx.clone());
                thread::spawn(move || {
                    let r = std::process::Command::new(bin).args(["stop", &short]).output();
                    let res = match r {
                        Ok(o) if o.status.success() => Ok(format!("stopped {short}")),
                        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(AppEvent::Stopped(res, then));
                });
            }
            Interrupt(id) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.conversation().as_deref() == Some(id.as_str())) {
                    s.interrupt();
                }
                self.flash("stopped");
            }
            Refused(why) => self.flash(why),
            Stay | Close => {}
        }
    }

    pub(crate) fn open_explorer(&mut self) {
        let open = self.open_states();
        let mut e = explorer::Explorer::from_cache(self.cwd(), open, self.list_cache.as_ref());
        self.load_sessions();
        e.set_folds(self.list_prefs.1.clone());
        // Start on the conversation the side panel is on, else the one in view.
        if let Some(id) = self.dock.as_ref().and_then(|d| d.selected()).map(|s| s.0).or_else(|| self.active_id()) {
            e.select(&id);
        }
        self.explorer = Some(e);
    }

    /// Read the session lists on a thread; `AppEvent::Sessions` brings them back. One read at a time.
    pub(crate) fn load_sessions(&mut self) {
        if self.list_loading || cfg!(test) {
            return;
        }
        self.list_loading = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(AppEvent::Sessions(explorer::load()));
        });
    }

    /// The conversation in view: the one selection every list agrees with.
    pub(crate) fn active_id(&self) -> Option<String> {
        self.sessions[self.active].conversation()
    }

    /// Reopen a stored conversation: switch to its tab if it is open, else use this tab when it is
    /// still blank, else a new one.
    pub(crate) fn open_session(&mut self, id: String, title: String, cwd: Option<String>) {
        if let Some(i) = self.sessions.iter().position(|s| s.conversation().as_deref() == Some(id.as_str())) {
            self.goto(i);
            return;
        }
        self.open_in_tab(id, title, cwd);
    }

    /// → in a list: the conversation takes the tab in front (its backend stops, as with `/clear`). One that
    /// is already open in another tab is switched to; a tab mid-turn is not thrown away.
    pub(crate) fn open_here(&mut self, id: String, title: String, cwd: Option<String>) {
        if self.sessions.iter().any(|s| s.conversation().as_deref() == Some(id.as_str())) {
            return self.open_session(id, title, cwd);
        }
        let s = &self.sessions[self.active];
        if s.busy || !s.pending.is_empty() {
            return self.open_session(id, title, cwd); // a working tab is not thrown away: new tab instead
        }
        self.s().clear();
        self.open_in_tab(id, title, cwd);
    }

    /// Resume `id` in this tab if it is blank, else in a new one, working in `cwd`.
    pub(crate) fn open_in_tab(&mut self, id: String, title: String, cwd: Option<String>) {
        let blank = {
            let s = &self.sessions[self.active];
            s.items.is_empty() && s.session_id.is_none() && s.backend.is_none()
        };
        if !blank {
            self.new_tab();
        }
        if !title.is_empty() {
            self.s().title = title.chars().take(18).collect();
        }
        // A conversation from another project starts with that project's profile, else the default one
        // (a tab you already set up by hand is left as it is).
        if !self.profile_forced && let Some(dir) = cwd.as_deref() {
            let own = project_profile(dir);
            if own.is_some() || !blank {
                let p = own.unwrap_or_else(|| self.default_profile.clone());
                self.s().profile = p;
            }
        }
        self.s().cwd = cwd;
        let tx = self.tx.clone();
        self.s().resume(id, &tx);
        self.files_at = None;
    }

    /// One selected conversation across the lists: the full screen one leads while it is open; the side panel
    /// leads while it has the keys; otherwise the panel rests on the conversation in view.
    pub fn sync_selection(&mut self) {
        let id = match (&self.explorer, self.dock_focus) {
            (Some(e), _) => e.selected().map(|s| s.0),
            (None, true) => return,
            (None, false) => {
                self.dock.iter_mut().for_each(explorer::Explorer::clear_filter);
                self.active_id()
            }
        };
        if let (Some(id), Some(d)) = (id, self.dock.as_mut()) {
            d.select(&id);
        }
    }

    /// Whichever list the key touched, make the other agree and save: folds and grouping outlive both.
    pub(crate) fn sync_list_prefs(&mut self) {
        let from_ui = self.explorer.as_ref().or(self.dock.as_ref()).map(|e| e.folds().to_vec());
        let prefs = (explorer::group(), from_ui.unwrap_or_else(|| self.list_prefs.1.clone()));
        if prefs == self.list_prefs {
            return;
        }
        self.list_prefs = prefs;
        for e in self.dock.iter_mut().chain(self.explorer.iter_mut()) {
            e.set_folds(self.list_prefs.1.clone());
        }
        if !cfg!(test) && let Some(p) = &self.config_path {
            let _ = config::set_str(p, "session_group", &self.list_prefs.0.to_string());
            let _ = config::set_value(p, "session_folds", serde_json::json!(self.list_prefs.1));
        }
    }
}
