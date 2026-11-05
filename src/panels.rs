//! Panels opened from the message box: usage, optimize, permissions, instructions, tab settings, config, pickers, rewind.

use super::*;

impl App {
    /// Ask Claude Code for the context breakdown and plan limits (free: no model call) and show the panel,
    /// which fills in when the replies arrive.
    pub(crate) fn open_usage(&mut self) {
        self.usage_open = true;
        self.ask_usage();
    }

    pub(crate) fn ask_usage(&mut self) {
        let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
        let s = self.s();
        s.usage = Default::default();
        s.warm(&launch, &tx);
        s.control("get_context_usage", json!({}));
        s.control("get_usage", json!({}));
    }

    /// `/optimize [level|off|now]`.
    pub(crate) fn optimize_cmd(&mut self, arg: &str) {
        let cwd = self.cwd().to_string();
        match arg {
            "off" => {
                let had = optimize::clear(&cwd);
                self.flash(if had { "optimize removed for this project: new tabs load everything again" } else { "nothing to remove: this project has no saved optimize settings" });
            }
            "now" => {
                let k = self.s().ctx_tokens / 1000;
                match self.s().restart_later() {
                    Ok(()) => {
                        self.s().unpinned = true;
                        self.s().note(format!("re-rendering this conversation with the project's optimize settings; the next message re-caches up to ~{k}k tokens"));
                    }
                    Err(e) => self.flash(e),
                }
            }
            _ => {
                let level = optimize::Level::parse(arg).or_else(|| optimize::load(&cwd).and_then(|o| o.level)).unwrap_or(optimize::Level::Balanced);
                if !arg.is_empty() && optimize::Level::parse(arg).is_none() {
                    return self.flash("usage: /optimize [light|balanced|aggressive|off|now]");
                }
                self.opt = Some(optimize::Ui::open(&cwd, level));
                self.ask_usage();
            }
        }
    }

    pub(crate) fn opt_poll(&mut self) {
        let Some(ui) = self.opt.as_mut() else { return };
        let u = &self.sessions[self.active].usage;
        let (skills, total) = u.ctx.as_ref().map_or((None, 0), |c| (Some(c.skill_list.as_slice()), c.total));
        ui.poll(skills, total);
        ui.poll_review();
    }

    pub(crate) fn opt_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.opt.take() else { return };
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) || ctrl_c {
            return;
        }
        if ui.loading {
            self.opt = Some(ui);
            return;
        }
        let n = ui.items.len();
        match k.code {
            KeyCode::Up | KeyCode::Down if n > 0 => ui.sel = panel::cycle(ui.sel, n, k.code == KeyCode::Up),
            KeyCode::Left | KeyCode::BackTab => ui.step_level(-1),
            KeyCode::Right | KeyCode::Tab => ui.step_level(1),
            KeyCode::Char('c') if !ctrl_c => {
                let cwd = self.cwd().to_string();
                ui.ask_claude(&self.launch.bin, &self.launch.env, &cwd);
            }
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                if let Some(it) = ui.items.get_mut(ui.sel) {
                    it.on = !it.on;
                }
            }
            KeyCode::Enter => {
                let cwd = self.cwd().to_string();
                match optimize::save(&cwd, &ui.saved()) {
                    Err(e) => self.flash(format!("could not save: {e}")),
                    Ok(()) => {
                        let started = self.sessions[self.active].ctx_tokens > 0;
                        let cold = self.sessions[self.active].last_req_at.is_none_or(|t| t.elapsed() > self.sessions[self.active].cache_ttl);
                        let msg = format!("optimize {} saved (≈ −{} at start). ", ui.level.name(), crate::usage::tokens(ui.saves()));
                        if !started || cold {
                            if self.s().restart_later().is_ok() {
                                self.s().unpinned = true;
                            }
                            self.flash(msg + "applied to this tab");
                        } else {
                            let k = self.s().ctx_tokens / 1000;
                            self.flash(msg + &format!("New tabs use it; this conversation keeps its prompt. `/optimize now` re-renders it (re-caches ~{k}k)"));
                        }
                        return;
                    }
                }
            }
            _ => {}
        }
        self.opt = Some(ui);
    }

    pub(crate) fn perms_key(&mut self, k: KeyEvent) {
        use permissions::Stage;
        let Some(mut ui) = self.perms.take() else { return };
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        ui.note = None;
        match ui.stage.clone() {
            Stage::Typing(mut text) => {
                match k.code {
                    KeyCode::Esc => ui.stage = Stage::List,
                    KeyCode::Enter if !text.trim().is_empty() => ui.stage = Stage::Where(text.trim().to_string(), 0),
                    KeyCode::Backspace => {
                        text.pop();
                        ui.stage = Stage::Typing(text);
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        text.push(c);
                        ui.stage = Stage::Typing(text);
                    }
                    _ => {}
                }
                if ctrl_c {
                    ui.stage = Stage::List;
                }
            }
            Stage::Where(rule, w) => match k.code {
                KeyCode::Esc => ui.stage = Stage::Typing(rule),
                KeyCode::Up => ui.stage = Stage::Where(rule, (w + 2) % 3),
                KeyCode::Down | KeyCode::Tab => ui.stage = Stage::Where(rule, (w + 1) % 3),
                KeyCode::Enter => {
                    let scope = permissions::Scope::ALL[w];
                    let res = permissions::Scope::file(scope, self.cwd()).ok_or("no settings location".to_string()).and_then(|f| permissions::add(&f, ui.tab, &rule));
                    match res {
                        Ok(true) => ui.stage = Stage::List,
                        Ok(false) => {
                            ui.note = Some(format!("{rule} is already in {} settings", scope.name()));
                            ui.stage = Stage::List;
                        }
                        Err(e) => ui.note = Some(e),
                    }
                    ui.reload(&self.sessions[self.active]);
                    ui.sel = ui.shown().iter().position(|r| r.text == rule).map_or(0, |i| i + 1);
                }
                _ => {}
            },
            stage => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => return,
                _ if ctrl_c => return,
                KeyCode::Left | KeyCode::BackTab => ui.switch(-1),
                KeyCode::Right | KeyCode::Tab => ui.switch(1),
                KeyCode::Up => {
                    ui.sel = (ui.sel + ui.shown().len()) % (ui.shown().len() + 1);
                    ui.stage = Stage::List;
                }
                KeyCode::Down => {
                    ui.sel = (ui.sel + 1) % (ui.shown().len() + 1);
                    ui.stage = Stage::List;
                }
                KeyCode::Enter if ui.sel == 0 => ui.stage = Stage::Typing(String::new()),
                KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace if ui.sel > 0 => {
                    if let Some(r) = ui.at().cloned() {
                        match r.source {
                            permissions::Source::Tab => ui.note = Some("this tab's own rule: change it in /session".into()),
                            permissions::Source::File(scope) if matches!(stage, Stage::Confirm(n) if n + 1 == ui.sel) => {
                                let res = scope.file(self.cwd()).ok_or("no settings location".to_string()).and_then(|f| permissions::remove(&f, r.behavior, &r.text));
                                if let Err(e) = res {
                                    ui.note = Some(e);
                                }
                                ui.stage = Stage::List;
                                ui.reload(&self.sessions[self.active]);
                            }
                            permissions::Source::File(_) => ui.stage = Stage::Confirm(ui.sel - 1),
                        }
                    }
                }
                _ => {}
            },
        }
        self.perms = Some(ui);
    }

    pub(crate) fn instr_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.instr.take() else { return };
        let rows = ui.rows();
        let sel = ui.sel.min(rows.len().saturating_sub(1));
        let close = matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL));
        if close {
            if ui.dirty {
                if let Some(f) = instructions::rules_path() {
                    instructions::save_rules(&f, &ui.rules);
                }
                // A tab that has not started yet picks the change up at once; a started one keeps the
                // prompt it began with (Claude Code pins it), so say so.
                if self.sessions[self.active].ctx_tokens == 0 && self.s().restart_later().is_ok() {
                    self.flash("instructions updated for this tab");
                } else {
                    self.flash("instructions updated: new tabs use them (this conversation keeps its start-up prompt)");
                }
            }
            return;
        }
        if let Some(&row) = rows.get(sel) {
            match k.code {
                KeyCode::Up | KeyCode::Down => ui.sel = panel::cycle(sel, rows.len(), k.code == KeyCode::Up),
                KeyCode::Right | KeyCode::Char('l') => {
                    if let instructions::Row::File(f) = row {
                        ui.open.insert(f);
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    let f = match row { instructions::Row::File(f) | instructions::Row::Sec(f, _) => f };
                    ui.open.remove(&f);
                    ui.sel = ui.rows().iter().position(|r| *r == instructions::Row::File(f)).unwrap_or(0);
                }
                KeyCode::Char('b') => ui.toggle_brief(row),
                KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Char('x') => {
                    if let Err(e) = ui.toggle(row) {
                        self.flash(e);
                    }
                }
                _ => {}
            }
        }
        self.instr = Some(ui);
    }

    pub(crate) fn tabcfg_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.tabcfg.take() else { return };
        let rows = tabcfg::rows(&self.sessions[self.active]);
        let sel = ui.sel.min(rows.len() - 1);
        if let Some(text) = ui.adding.as_mut() {
            match k.code {
                KeyCode::Esc => ui.adding = None,
                KeyCode::Enter => {
                    let rule = text.trim().to_string();
                    ui.adding = None;
                    if !rule.is_empty() {
                        self.s().set_rule(&rule, Some(true));
                    }
                }
                KeyCode::Backspace => drop(text.pop()),
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                _ => {}
            }
            self.tabcfg = Some(ui);
            return;
        }
        let delta = match k.code {
            KeyCode::Left | KeyCode::BackTab => -1,
            KeyCode::Right | KeyCode::Tab | KeyCode::Char(' ') => 1,
            _ => 0,
        };
        match (k.code, &rows[sel]) {
            (KeyCode::Esc, _) => return,
            (KeyCode::Char('c'), _) if k.modifiers.contains(KeyModifiers::CONTROL) => return,
            (KeyCode::Up | KeyCode::Down, _) => ui.sel = panel::cycle(sel, rows.len(), k.code == KeyCode::Up),
            (KeyCode::Enter, Row::Add) | (_, Row::Add) if delta != 0 || k.code == KeyCode::Enter => ui.adding = Some(String::new()),
            (_, Row::Mode) if delta != 0 || k.code == KeyCode::Enter => {
                let modes = tabcfg::modes();
                let cur = modes.iter().position(|m| *m == self.sessions[self.active].mode).map_or(0, |i| i as isize);
                let next = modes[(cur + if delta == 0 { 1 } else { delta }).rem_euclid(modes.len() as isize) as usize];
                self.set_mode(next);
            }
            (KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace, Row::Tool(name, _)) => self.s().set_rule(name, None),
            (_, Row::Tool(name, _)) if delta != 0 || k.code == KeyCode::Enter => {
                let now = self.sessions[self.active].rule(name);
                self.s().set_rule(name, tabcfg::step(now, if delta == 0 { 1 } else { delta }));
            }
            _ => {}
        }
        ui.sel = ui.sel.min(tabcfg::rows(&self.sessions[self.active]).len() - 1);
        self.tabcfg = Some(ui);
    }

    pub(crate) fn set_mode(&mut self, mode: &str) {
        let s = self.s();
        s.control("set_permission_mode", json!({"mode": mode}));
        s.mode = mode.into();
        s.status_dirty = true;
    }

    pub(crate) fn settings_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.settings.take() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return, // closed
            KeyCode::Char('c') if ctrl => return,
            KeyCode::Up => ui.step(-1),
            KeyCode::Down => ui.step(1),
            KeyCode::PageUp => ui.step(-10),
            KeyCode::PageDown => ui.step(10),
            KeyCode::Left => self.change_setting(ui.sel, -1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.change_setting(ui.sel, 1),
            _ => {}
        }
        self.settings = Some(ui);
    }

    /// Colours changed: drop every rendered line that baked the old ones in, and repaint the whole screen.
    pub(crate) fn restyle(&mut self) {
        for s in &mut self.sessions {
            s.items.iter_mut().for_each(|i| i.cache = None);
        }
        self.clear_request = true;
    }

    /// What a setting is right now (the live value, which a flag may have set).
    pub fn setting_value(&self, key: &str) -> String {
        match key {
            "statusline" => self.statusline_mode.name().to_string(),
            "profile" => self.default_profile.clone(),
            "images" => image::mode().to_string(),
            "cache_ttl_min" => (self.cache_ttl.as_secs() / 60).to_string(),
            "idle_suspend_s" => self.idle_suspend.as_secs().to_string(),
            "autocompact_k" => self.autocompact_k.to_string(),
            "mem_budget_mb" => (self.mem_budget_kb / 1024).to_string(),
            "notify" => self.notify.clone(),
            "mouse" => if self.mouse { "on" } else { "off" }.to_string(),
            "autocopy" => if self.autocopy { "on" } else { "off" }.to_string(),
            "side_panel" => if self.dock_mode { "on" } else { "off" }.to_string(),
            "input_mode" => if self.vim.on { "vim" } else if self.editor_mode { "editor" } else { "default" }.to_string(),
            "theme" => theme::mode_name().to_string(),
            "queue" => if self.queue_on { "on" } else { "off" }.to_string(),
            "share_mcp" => if self.share_mcp { "on" } else { "off" }.to_string(),
            _ => String::new(),
        }
    }

    pub(crate) fn change_setting(&mut self, index: usize, delta: isize) {
        let item = &settings::ITEMS[index];
        let value = settings::cycle(item, &self.setting_value(item.key), delta);
        self.apply_setting(item.key, value);
    }

    /// Apply a setting now and remember it in the config file.
    pub(crate) fn apply_setting(&mut self, key: &str, value: &str) {
        let num = value.parse::<u64>().unwrap_or(0);
        match key {
            "statusline" => {
                if let Some(note) = self.apply_statusline_mode(statusline::Mode::parse(value).unwrap_or(statusline::Mode::Builtin)) {
                    self.flash(format!("{note}; showing claude++'s own line"));
                }
            }
            "profile" => self.default_profile = value.into(),
            "images" => image::set_mode(value),
            "cache_ttl_min" => {
                self.cache_ttl = Duration::from_secs(num * 60);
                let ttl = self.cache_ttl;
                self.sessions.iter_mut().for_each(|s| s.cache_ttl = ttl);
            }
            "idle_suspend_s" => self.idle_suspend = Duration::from_secs(num),
            "autocompact_k" => self.autocompact_k = num,
            "mem_budget_mb" => self.mem_budget_kb = num * 1024,
            "mouse" => self.set_mouse(value != "off"),
            "autocopy" => self.autocopy = value == "on",
            "notify" => self.notify = value.into(),
            "theme" => {
                theme::set_mode(value);
                self.restyle();
            }
            "input_mode" => {
                self.vim.on = value == "vim";
                self.editor_mode = value == "editor";
                self.vim.mode = vim::Mode::Insert;
                self.vim_shape();
            }
            "queue" => self.queue_on = value == "on",
            "share_mcp" => self.share_mcp = value == "on",
            "side_panel" => {
                self.dock_mode = value == "on";
                if !self.dock_mode {
                    (self.dock, self.dock_focus) = (None, false);
                }
            }
            _ => return,
        }
        if let Some(p) = self.config_path.clone()
            && let Err(e) = config::set_str(&p, key, value)
        {
            self.flash(format!("could not save {}: {e}", p.display()));
        }
    }

    pub(crate) fn open_model_picker(&mut self) {
        if self.model_list().is_empty() {
            // Nothing remembered yet: start the backend so the list arrives.
            let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
            self.s().warm(&launch, &tx);
        }
        let s = &self.sessions[self.active];
        let sel = self.current_model().unwrap_or(0);
        let effort = self.model_list().get(sel).and_then(|m| m.efforts.iter().position(|e| *e == s.effort)).unwrap_or(0);
        let fast = s.fast_mode || s.fast_pref == Some(true);
        self.model_picker = Some(ModelPicker { sel, effort, fast });
    }

    /// The colour the whole UI wears: the highlighted one while `/color` is open, else that of the conversation
    /// being browsed in the session list, else that of the tab in front.
    pub fn accent_color(&mut self) -> Option<String> {
        if let Some(c) = self.picked_color() {
            return c;
        }
        let list = if self.explorer.is_some() { self.explorer.as_mut() } else if self.dock_focus { self.dock.as_mut() } else { None };
        if let Some(list) = list
            && let Some((id, _)) = list.selected()
        {
            return match self.sessions.iter().find(|s| s.session_id.as_deref() == Some(id.as_str())) {
                Some(s) => s.color.clone(),
                None => list.color(),
            };
        }
        self.sessions[self.active].color.clone()
    }

    /// Tab `i`'s colour; the tab in front shows the colour being picked.
    pub fn tab_color(&self, i: usize) -> Option<&str> {
        match self.color_picker.and_then(|p| Self::color_choices().get(p).copied()) {
            Some(c) if i == self.active => Some(c).filter(|c| *c != "default"),
            _ => self.sessions[i].color.as_deref(),
        }
    }

    /// While `/color` is open: the highlighted choice (`None` inside is `default`).
    pub(crate) fn picked_color(&self) -> Option<Option<String>> {
        let c = *Self::color_choices().get(self.color_picker?)?;
        Some((c != "default").then(|| c.to_string()))
    }

    /// The picker's rows: `default`, then each colour.
    pub fn color_choices() -> Vec<&'static str> {
        std::iter::once("default").chain(theme::color_names()).collect()
    }

    pub(crate) fn color_key(&mut self, k: KeyEvent) {
        let Some(sel) = self.color_picker else { return };
        let n = Self::color_choices().len();
        match k.code {
            KeyCode::Esc => self.color_picker = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.color_picker = None,
            KeyCode::Up | KeyCode::Down => self.color_picker = Some(panel::cycle(sel, n, k.code == KeyCode::Up)),
            KeyCode::Enter => {
                self.color_picker = None;
                // Through the normal path, so Claude Code records it with the conversation.
                self.input = format!("/color {}", Self::color_choices()[sel]);
                self.cursor = self.input.chars().count();
                self.submit();
            }
            _ => {}
        }
    }

    pub(crate) fn open_rewind(&mut self) {
        let n = self.s().rewind_points().len();
        if n == 0 {
            return self.flash("nothing to rewind to");
        }
        self.flash = None;
        self.rewind = Some(RewindPicker { sel: n - 1, restore: 0 });
    }

    pub(crate) fn rewind_key(&mut self, k: KeyEvent) {
        let Some(mut p) = self.rewind else { return };
        let points = self.s().rewind_points();
        let n = points.len();
        if n == 0 {
            return self.rewind = None;
        }
        p.sel = p.sel.min(n - 1);
        let choices = RewindPicker::choices(self.s().edits_since(points[p.sel].0));
        match k.code {
            KeyCode::Esc => return self.rewind = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return self.rewind = None,
            KeyCode::Up | KeyCode::Down => p.sel = panel::cycle(p.sel, n, k.code == KeyCode::Up),
            KeyCode::Left => p.restore = p.restore.saturating_sub(1),
            KeyCode::Right => p.restore += 1,
            KeyCode::Enter => {
                self.rewind = None;
                let (_, conversation, code) = choices[p.restore.min(choices.len() - 1)];
                let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
                match self.s().rewind(points[p.sel].0, conversation, code, &launch, &tx) {
                    Ok(Some(text)) => self.set_input(text),
                    Ok(None) => {}
                    Err(e) => self.flash(&e),
                }
                return;
            }
            _ => return,
        }
        // Each message offers its own choices; keep the cursor on one that exists.
        p.restore = p.restore.min(RewindPicker::choices(self.s().edits_since(points[p.sel].0)).len() - 1);
        self.rewind = Some(p);
    }

    pub(crate) fn picker_key(&mut self, k: KeyEvent) {
        let n = self.model_list().len();
        let Some(mut p) = self.model_picker else { return };
        // The list can shrink under an open picker (the backend's own list replaces the cached one).
        p.sel = p.sel.min(n.saturating_sub(1));
        let levels = |sel: usize| self.model_list().get(sel).map_or(0, |m| m.efforts.len());
        let next = match k.code {
            KeyCode::Esc => return self.model_picker = None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return self.model_picker = None,
            KeyCode::Enter => return self.apply_model_picker(),
            KeyCode::Up | KeyCode::Down if n > 0 => {
                let new = panel::cycle(p.sel, n, k.code == KeyCode::Up);
                // Keep the same level name when the next model offers it.
                let name = self.model_list()[p.sel].efforts.get(p.effort).cloned();
                let keep = name.and_then(|e| self.model_list()[new].efforts.iter().position(|x| *x == e));
                ModelPicker { sel: new, effort: keep.unwrap_or(p.effort.min(levels(new).saturating_sub(1))), ..p }
            }
            KeyCode::Left => ModelPicker { effort: p.effort.saturating_sub(1), ..p },
            KeyCode::Right => ModelPicker { effort: (p.effort + 1).min(levels(p.sel).saturating_sub(1)), ..p },
            // Space (as in Claude Code's /fast) or f toggles fast mode, for models that have it.
            KeyCode::Char(' ' | 'f') if self.model_list().get(p.sel).is_some_and(|m| m.fast) => ModelPicker { fast: !p.fast, ..p },
            _ => return,
        };
        self.model_picker = Some(next);
    }

    pub(crate) fn apply_model_picker(&mut self) {
        let Some(p) = self.model_picker.take() else { return };
        let Some(m) = self.model_list().get(p.sel).cloned() else { return };
        let level = m.efforts.get(p.effort).cloned();
        let model_changed = self.current_model() != Some(p.sel);
        let s = &self.sessions[self.active];
        let wants_fast = p.fast && m.fast;
        let had_fast = s.fast_mode || s.fast_pref == Some(true);
        let effort_changed = level.as_ref().is_some_and(|l| *l != s.effort);
        let fast_changed = wants_fast != had_fast;
        if !model_changed && !effort_changed && !fast_changed {
            return;
        }
        let warn = if model_changed { self.s().recache_warning() } else { String::new() };
        let k = self.s().ctx_tokens / 1000;
        if model_changed {
            self.s().choose_model(&m.value);
        }
        let mut what = vec![m.name.clone()];
        if let Some(l) = level.filter(|_| model_changed || effort_changed) {
            self.s().choose_effort(&l);
            what.push(format!("effort {l}"));
        }
        // Turning it on (or keeping it on across a model switch) is sent; turning it off is sent unless
        // choose_model already did so because the new model lacks fast mode.
        if wants_fast && (fast_changed || model_changed) {
            self.s().choose_fast(true);
        } else if !wants_fast && had_fast && self.s().fast_pref != Some(false) {
            self.s().choose_fast(false);
        }
        if wants_fast && fast_changed {
            what.push("fast ↯".into());
            let cost = if k > 0 { format!(" The first fast request re-reads the whole conversation (~{k}k tokens) at fast pricing, once.") } else { String::new() };
            self.s().note(format!("{}. Fast mode costs more per token and is billed from usage credits.{cost}{warn}", what.join(" · ")));
        } else {
            self.s().note(format!("{}.{warn}", what.join(" · ")));
        }
    }
}
