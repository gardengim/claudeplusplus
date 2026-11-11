//! Panels opened from the message box: usage, optimize, permissions, instructions, tab settings, config, pickers, rewind.

use super::*;

/// Every full panel that takes over the keys and the screen's bottom. Declared in the order they win when
/// several are open: keys, drawing and `overlay_open` all follow this one list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Panel {
    Explorer,
    Color,
    Model,
    Rewind,
    Settings,
    Items,
    Open,
    Worktree,
    Term,
    TabCfg,
    Instr,
    Perms,
    Opt,
    Ctx,
    Procs,
    Lanes,
    Sched,
    Queue,
    Search,
    Usage,
    Supervisor,
}

impl App {
    /// The panel in front, if any.
    pub(crate) fn front_panel(&self) -> Option<Panel> {
        let open = [
            (self.explorer.is_some(), Panel::Explorer),
            (self.color_picker.is_some(), Panel::Color),
            (self.model_picker.is_some(), Panel::Model),
            (self.rewind.is_some(), Panel::Rewind),
            (self.settings.is_some(), Panel::Settings),
            (self.items_ui.is_some(), Panel::Items),
            (self.open_ui.is_some(), Panel::Open),
            (self.wt_ui.is_some(), Panel::Worktree),
            (self.term_ui.is_some(), Panel::Term),
            (self.tabcfg.is_some(), Panel::TabCfg),
            (self.instr.is_some(), Panel::Instr),
            (self.perms.is_some(), Panel::Perms),
            (self.opt.is_some(), Panel::Opt),
            (self.ctx_ui.is_some(), Panel::Ctx),
            (self.procs_ui.is_some(), Panel::Procs),
            (self.lanes.is_some(), Panel::Lanes),
            (self.sched_ui.is_some(), Panel::Sched),
            (self.queue_ui.is_some(), Panel::Queue),
            (self.search.is_some(), Panel::Search),
            (self.usage_open, Panel::Usage),
            (self.sup_ui.is_some(), Panel::Supervisor),
        ];
        open.into_iter().find(|p| p.0).map(|p| p.1)
    }
}

impl App {
    /// Ask Claude Code for the context breakdown and plan limits (free: no model call) and show the panel,
    /// which fills in when the replies arrive.
    pub(crate) fn open_usage(&mut self) {
        self.usage_open = true;
        self.ask_usage();
    }

    /// `/procs`: open the panel at once and fill it in when the process list has been read (off the UI thread).
    pub(crate) fn open_supervisor(&mut self) {
        self.sup_ui = Some(Default::default());
        if let Some(c) = self.sup.as_ref().filter(|c| c.alive() && c.manages()) {
            c.ask_status();
        }
    }

    /// How this window stands with the supervisor, and its pid when connected.
    pub(crate) fn sup_link(&self) -> (supui::Link, Option<u32>) {
        // The window's own supervisor is not the one this panel is about.
        match self.sup.as_ref().filter(|c| c.alive() && !c.is_private()) {
            Some(c) if c.manages() => (supui::Link::Live, c.pid()),
            Some(c) => (supui::Link::Old, c.pid()),
            None => {
                let env = std::env::var("CLAUDEPP_SUPERVISOR").is_ok_and(|v| v == "off");
                (supui::Link::Down { stopped: self.sup_paths.stopped_marker().exists(), env }, None)
            }
        }
    }

    /// What setting `i` of the supervisor is now: its own word while it answers, else what is saved.
    pub(crate) fn sup_setting(&self, i: usize) -> String {
        self.sup_setting_in(i, self.sup_ui.as_ref().and_then(|ui| ui.status.as_ref()))
    }

    fn sup_setting_in(&self, i: usize, status: Option<&serde_json::Value>) -> String {
        let s = &supui::SETTINGS[i];
        let live = status.map(|st| &st["settings"][s.key]).filter(|v| !v.is_null()).filter(|_| self.sup_link().0 == supui::Link::Live);
        if let Some(v) = live {
            return match v {
                serde_json::Value::Bool(b) => match (s.key, b) {
                    ("budget", false) => "paused".into(),
                    (_, true) => "on".into(),
                    _ => "off".into(),
                },
                v => v.as_str().map_or_else(|| v.to_string(), String::from),
            };
        }
        let saved = self.config_path.as_deref().and_then(|p| config::get_str(p, &format!("supervisor_{}", s.key)));
        saved.unwrap_or_else(|| s.choices[0].0.to_string())
    }

    pub(crate) fn sup_key(&mut self, k: KeyEvent) {
        use supui::Act;
        let Some(mut ui) = self.sup_ui.take() else { return };
        let (link, _) = self.sup_link();
        let rows = supui::rows(link, ui.status.as_ref().filter(|_| link == supui::Link::Live));
        let act = ui.key(k, link, &rows);
        let paths = self.sup_paths.clone();
        let mut keep = true;
        match act {
            Act::None => {}
            Act::Close => keep = false,
            Act::Change(i, delta) => {
                let s = &supui::SETTINGS[i];
                let now = self.sup_setting_in(i, ui.status.as_ref());
                let at = s.choices.iter().position(|c| c.0 == now).map_or(-1, |p| p as isize);
                let next = s.choices[(at + delta).rem_euclid(s.choices.len() as isize) as usize].0;
                if let Some(p) = self.config_path.clone()
                    && let Err(e) = config::set_str(&p, &format!("supervisor_{}", s.key), next)
                {
                    self.flash(format!("could not save {}: {e}", p.display()));
                }
                if let Some(c) = self.sup.as_ref().filter(|c| c.alive() && c.manages()) {
                    c.set(s.key, next);
                }
            }
            Act::McpRestart(i) => {
                let key = ui.status.as_ref().and_then(|s| s["mcp"][i]["key"].as_str().map(String::from));
                if let (Some(key), Some(c)) = (key, self.sup.as_ref()) {
                    c.restart_mcp(&key);
                    self.flash("restarting the MCP server: the next message starts it again");
                    ui.asked = Instant::now() - Duration::from_secs(60); // look again at the next tick
                }
            }
            Act::Restart | Act::Stop => {
                let stop = matches!(act, Act::Stop);
                if !stop {
                    supervisor::allow_start(&paths);
                }
                if let Some(e) = stop.then(|| supervisor::stop_on_purpose(&paths)).and_then(Result::err) {
                    self.flash(format!("could not stop it: {e}"));
                } else {
                    let sent = match self.sup.as_ref().filter(|c| c.alive()) {
                        Some(c) if c.manages() => {
                            c.shutdown();
                            true
                        }
                        Some(c) => c.pid().is_some_and(supervisor::terminate_old),
                        None => true,
                    };
                    if sent {
                        // Windows notice the lost connection and, unless it was stopped on purpose, start a new one.
                        self.sup_tried = Instant::now().checked_sub(Duration::from_secs(8)).unwrap_or_else(Instant::now);
                        ui.status = None;
                        self.flash(if stop { "supervisor stopped; /supervisor starts it again" } else { "restarting the supervisor…" });
                    } else {
                        self.flash("could not stop the supervisor: its process is not one claude++ can identify as its own");
                    }
                }
            }
            Act::Start => {
                supervisor::allow_start(&paths);
                self.sup_tried = Instant::now().checked_sub(Duration::from_secs(60)).unwrap_or_else(Instant::now);
                self.flash("starting the supervisor…");
            }
        }
        if keep {
            self.sup_ui = Some(ui);
        }
    }

    /// While `/supervisor` is open: ask it again every second, so the numbers move.
    pub(crate) fn refresh_supervisor(&mut self) {
        if let Some(ui) = self.sup_ui.as_mut()
            && ui.asked.elapsed() > Duration::from_secs(1)
        {
            ui.asked = Instant::now();
            if let Some(c) = self.sup.as_ref().filter(|c| c.alive() && c.manages()) {
                c.ask_status();
            }
        }
    }

    pub(crate) fn open_procs(&mut self) {
        self.procs_ui = Some(Default::default());
        self.read_procs();
    }

    pub(crate) fn read_procs(&mut self) {
        if let Some(ui) = self.procs_ui.as_mut() {
            ui.set(None);
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(AppEvent::Procs(procs::report()));
        });
    }

    pub(crate) fn ask_usage(&mut self) {
        let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
        let s = self.s();
        s.usage = Default::default();
        s.warm(&launch, &tx);
        s.control("get_context_usage", json!({}));
        s.control("get_usage", json!({}));
    }

    /// `/btw <question>`: ask in the background and show the question in the transcript, where the answer
    /// fills in. Nothing opens and nothing is interrupted, so it works while Claude is mid-turn, and several
    /// can be out at once.
    pub(crate) fn btw_cmd(&mut self, question: &str) {
        if question.is_empty() {
            return self.flash("usage: /btw <question>");
        }
        let launch = self.launch_for(self.active);
        let (cwd, tx) = (self.cwd().to_string(), self.tx.clone());
        self.btw_seq += 1;
        let id = self.btw_seq;
        let s = self.s();
        let (tab, args) = (s.id, btw::args(s.session_id.as_deref(), &s.model));
        s.start_side(id, question);
        let question = question.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(AppEvent::Btw(tab, id, btw::ask(&launch.bin, &launch.env, &cwd, &args, &question)));
        });
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
        let ctrl_c = crate::panel::is_ctrl_c(&k);
        if k.code == KeyCode::Esc || ctrl_c {
            return;
        }
        if ui.loading {
            self.opt = Some(ui);
            return;
        }
        let n = ui.items.len();
        // Two menus, one with the keys: the level (left and right) and the items (up and down).
        if let Some(to) = panel::menu_after(&k, ui.on_level as usize, 2) {
            ui.on_level = to == 1;
        }
        match k.code {
            KeyCode::Left | KeyCode::Right if ui.on_level => ui.step_level(if k.code == KeyCode::Left { -1 } else { 1 }),
            KeyCode::Up | KeyCode::Down if n > 0 && !ui.on_level => ui.sel = panel::cycle(ui.sel, n, k.code == KeyCode::Up),
            KeyCode::Char('c') if !ctrl_c => {
                let cwd = self.cwd().to_string();
                ui.ask_claude(&self.launch.bin, &self.launch.env, &cwd);
            }
            KeyCode::Char(' ') | KeyCode::Char('x') if !ui.on_level => {
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
        let ctrl_c = crate::panel::is_ctrl_c(&k);
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
                KeyCode::Down => ui.stage = Stage::Where(rule, (w + 1) % 3),
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
                KeyCode::Esc => return,
                _ if ctrl_c => return,
                // Two menus, one with the keys: the allow/ask/deny strip and the rules.
                KeyCode::Tab | KeyCode::BackTab => ui.on_strip = !ui.on_strip,
                KeyCode::Left | KeyCode::Right if ui.on_strip => ui.switch(if k.code == KeyCode::Left { -1 } else { 1 }),
                _ if ui.on_strip => {}
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
        let close = k.code == KeyCode::Esc || crate::panel::is_ctrl_c(&k);
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
            KeyCode::Left => -1,
            KeyCode::Right | KeyCode::Char(' ') => 1,
            _ => 0,
        };
        match (k.code, &rows[sel]) {
            (KeyCode::Esc, _) => return,
            (KeyCode::Char('c'), _) if k.modifiers.contains(KeyModifiers::CONTROL) => return,
            (KeyCode::Up | KeyCode::Down, _) => ui.sel = panel::cycle(sel, rows.len(), k.code == KeyCode::Up),
            (KeyCode::Char(c @ ('c' | 'p')), _) => {
                let info = tabcfg::info(&self.sessions[self.active], self.cwd());
                let (what, value) = if c == 'c' { ("session id", info[0].1.clone()) } else { ("transcript path", info[6].1.clone()) };
                if value.starts_with('—') {
                    self.flash(format!("no {what} yet: send a message first"));
                } else {
                    self.copy_text(&value);
                    self.flash(format!("copied the {what}"));
                }
            }
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
            // Two menus, one with the keys: the sections (a strip) and the rows (a list, each with its choices).
            KeyCode::Tab | KeyCode::BackTab => ui.strip = !ui.strip,
            KeyCode::Left | KeyCode::Right if ui.strip => ui.switch(if k.code == KeyCode::Left { -1 } else { 1 }),
            _ if ui.strip => {}
            KeyCode::Up => ui.step(-1),
            KeyCode::Down => ui.step(1),
            KeyCode::PageUp => ui.step(-10),
            KeyCode::PageDown => ui.step(10),
            KeyCode::Left => self.change_setting(ui.current(), -1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => self.change_setting(ui.current(), 1),
            _ => {}
        }
        self.settings = Some(ui);
    }

    pub(crate) fn items_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.items_ui.take() else { return };
        let (ctrl, alt) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT));
        let mut changed = true;
        match k.code {
            KeyCode::Esc => return, // closed
            KeyCode::Char('c') if ctrl => return,
            KeyCode::Up if alt => ui.shift(&mut self.line_items, -1),
            KeyCode::Down if alt => ui.shift(&mut self.line_items, 1),
            KeyCode::Up | KeyCode::Down => {
                ui.step(if k.code == KeyCode::Up { -1 } else { 1 });
                changed = false;
            }
            KeyCode::Char(' ') | KeyCode::Enter => ui.toggle(&mut self.line_items),
            KeyCode::Char('r') => {
                self.line_items = segments::DEFAULT.to_vec();
                ui.sel = 0;
            }
            _ => changed = false,
        }
        if changed && let Some(p) = &self.config_path {
            let _ = config::set_str(p, "statusline_items", &self.line_items.join(","));
        }
        self.items_ui = Some(ui);
    }

    pub(crate) fn wt_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.wt_ui.take() else { return };
        if let Some(text) = ui.adding.as_mut() {
            match k.code {
                KeyCode::Esc => ui.adding = None,
                KeyCode::Enter => {
                    let name = std::mem::take(text);
                    ui.adding = None;
                    self.wt_add(name.trim(), None);
                }
                KeyCode::Backspace => drop(text.pop()),
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                _ => {}
            }
            self.wt_ui = Some(ui);
            return;
        }
        let n = ui.list.len().max(1);
        let sel = ui.sel.min(n - 1);
        match k.code {
            KeyCode::Esc => return, // closed
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return,
            KeyCode::Up | KeyCode::Down => {
                ui.sel = panel::cycle(sel, n, k.code == KeyCode::Up);
                ui.armed = None;
            }
            KeyCode::Enter | KeyCode::Char('c') => {
                if let Some(w) = ui.list.get(sel) {
                    if w.prunable {
                        self.flash("that worktree's folder is missing; `git worktree prune` clears it");
                    } else if k.code == KeyCode::Enter {
                        self.open_folder_tab(&w.path.clone());
                    } else {
                        self.cd_cmd(&w.path.clone());
                    }
                }
                return;
            }
            KeyCode::Char('n') => ui.adding = Some(String::new()),
            KeyCode::Char('d') | KeyCode::Delete => match ui.list.get(sel) {
                Some(_) if sel == 0 => self.flash("that is the main worktree"),
                Some(w) if ui.armed == Some(sel) => {
                    ui.armed = None;
                    self.wt_remove(&w.path.clone());
                }
                Some(_) => ui.armed = Some(sel),
                None => {}
            },
            _ => {}
        }
        self.wt_ui = Some(ui);
    }

    pub(crate) fn open_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.open_ui.take() else { return };
        let n = ui.found.len().max(1);
        let sel = ui.sel.min(n - 1);
        match k.code {
            KeyCode::Esc => return, // closed
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return,
            KeyCode::Up => ui.sel = panel::cycle(sel, n, true),
            KeyCode::Down => ui.sel = panel::cycle(sel, n, false),
            KeyCode::Enter => {
                if let Some(f) = ui.found.get(sel) {
                    self.open_found(f);
                }
                return;
            }
            KeyCode::Char('e') => {
                match ui.found.get(sel).map(|f| &f.target) {
                    Some(open::Target::Path(p)) if p.is_file() => self.edit_file = Some(p.clone()),
                    _ => self.flash("e edits a file; this is a link or a folder"),
                }
                return;
            }
            _ => {}
        }
        self.open_ui = Some(ui);
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
            "spinner" | "progress" | "motion" => motion::look().raw(key).to_string(),
            "cache_ttl_min" => (self.cache_ttl.as_secs() / 60).to_string(),
            "idle_suspend_s" => self.limit(1, key, 120).to_string(),
            "autocompact_k" => self.autocompact_k.to_string(),
            "precompact_k" => self.precompact_k.to_string(),
            "mem_budget_mb" => self.limit(0, key, 0).to_string(),
            "notify" => self.notify.clone(),
            "mouse" => if self.mouse { "on" } else { "off" }.to_string(),
            "side_panel" => if self.dock_mode { "on" } else { "off" }.to_string(),
            "input_mode" => if self.vim.on { "vim" } else if self.editor_mode { "editor" } else { "default" }.to_string(),
            "theme" => theme::mode_name().to_string(),
            "queue" => if self.queue_on { "on" } else { "off" }.to_string(),
            "limit_warn" => self.limit_warn.to_string(),
            "limit_hold" => self.limit_hold.to_string(),
            "share_mcp" => if self.share_mcp { "on" } else { "off" }.to_string(),
            "agent_term" => if self.agent_term { "on" } else { "off" }.to_string(),
            "shell" => if self.shell_login { "login" } else { "sh" }.to_string(),
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
            "spinner" | "progress" | "motion" => motion::set(key, value),
            "cache_ttl_min" => {
                self.cache_ttl = Duration::from_secs(num * 60);
                let ttl = self.cache_ttl;
                self.sessions.iter_mut().for_each(|s| s.cache_ttl = ttl);
            }
            "idle_suspend_s" => {
                self.limits.1 = Some(num);
                self.send_limits();
            }
            "autocompact_k" => self.autocompact_k = num,
            "precompact_k" => self.precompact_k = num,
            "mem_budget_mb" => {
                self.limits.0 = Some(num);
                self.send_limits();
            }
            "notify" => self.notify = value.into(),
            "mouse" => self.set_mouse(value != "off"),
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
            "limit_warn" => self.limit_warn = num.min(100) as u8,
            "limit_hold" => {
                self.limit_hold = num.min(100) as u8;
                self.limit_released = None;
            }
            "share_mcp" => self.share_mcp = value == "on",
            "agent_term" => self.agent_term = value == "on",
            "shell" => self.shell_login = value != "sh",
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
        self.model_picker = Some(ModelPicker { sel, effort, fast, focus: 0 });
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
        self.rewind = Some(RewindPicker { sel: n - 1, restore: 0, on_restore: false });
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
            KeyCode::Tab | KeyCode::BackTab if choices.len() > 1 => p.on_restore = !p.on_restore,
            KeyCode::Up | KeyCode::Down if !p.on_restore => p.sel = panel::cycle(p.sel, n, k.code == KeyCode::Up),
            KeyCode::Left | KeyCode::Right if p.on_restore => p.restore = panel::cycle(p.restore.min(choices.len() - 1), choices.len(), k.code == KeyCode::Left),
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

    /// `/model`: four menus and exactly one has the keys (Tab moves it). Models and versions are vertical lists
    /// (up and down), effort and fast mode are rows of choices (left and right).
    pub(crate) fn picker_key(&mut self, k: KeyEvent) {
        let n = self.model_list().len();
        let Some(mut p) = self.model_picker else { return };
        // The list can shrink under an open picker (the backend's own list replaces the cached one).
        p.sel = p.sel.min(n.saturating_sub(1));
        let levels = |sel: usize| self.model_list().get(sel).map_or(0, |m| m.efforts.len());
        // Move to another model, keeping the same effort level by name when it offers it.
        let retarget = |p: ModelPicker, new: usize| {
            let list = self.model_list();
            let keep = list[p.sel].efforts.get(p.effort).and_then(|e| list[new].efforts.iter().position(|x| x == e));
            ModelPicker { sel: new, effort: keep.unwrap_or(p.effort.min(levels(new).saturating_sub(1))), ..p }
        };
        let usable = ModelPicker::usable(self.model_list(), p.sel);
        let focus = p.focused(usable);
        let up = k.code == KeyCode::Up || k.code == KeyCode::Left;
        let next = match (focus, k.code) {
            (_, KeyCode::Esc) => return self.model_picker = None,
            (_, KeyCode::Char('c')) if k.modifiers.contains(KeyModifiers::CONTROL) => return self.model_picker = None,
            (_, KeyCode::Enter) => return self.apply_model_picker(),
            (_, KeyCode::Tab | KeyCode::BackTab) => {
                let mut to = focus;
                for _ in 0..4 {
                    to = panel::menu_after(&k, to, 4).unwrap_or(to);
                    if usable[to] {
                        break;
                    }
                }
                ModelPicker { focus: to as u8, ..p }
            }
            // Models: up and down. It lands on the same version when the family has it, else on its newest.
            (0, KeyCode::Up | KeyCode::Down) if n > 0 => {
                let fams = crate::session::families(self.model_list());
                let at = fams.iter().position(|f| f.1.contains(&p.sel)).unwrap_or(0);
                let to = &fams[panel::cycle(at, fams.len(), up)].1;
                let version = self.model_list()[p.sel].parts().1;
                let new = to.iter().copied().find(|&i| version.is_some() && self.model_list()[i].parts().1 == version).unwrap_or(to[0]);
                retarget(p, new)
            }
            // Versions of the family in the first menu: up and down.
            (1, KeyCode::Up | KeyCode::Down) if n > 0 => {
                let fams = crate::session::families(self.model_list());
                let versions = fams.iter().find(|f| f.1.contains(&p.sel)).map_or(vec![p.sel], |f| f.1.clone());
                let at = versions.iter().position(|&i| i == p.sel).unwrap_or(0);
                retarget(p, versions[panel::cycle(at, versions.len(), up)])
            }
            (2, KeyCode::Left | KeyCode::Right) if levels(p.sel) > 0 => ModelPicker { effort: panel::cycle(p.effort, levels(p.sel), up), ..p },
            (3, KeyCode::Left | KeyCode::Right) => ModelPicker { fast: !up, ..p },
            (3, KeyCode::Char(' ' | 'f')) => ModelPicker { fast: !p.fast, ..p },
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

impl App {
    pub(crate) fn open_search(&mut self) {
        let scroll = self.s().scroll;
        let mut ui = search::Ui::new(scroll);
        ui.refresh(&self.sessions[self.active].items, &self.history);
        self.search = Some(ui);
    }

    pub(crate) fn search_key(&mut self, k: KeyEvent) {
        let Some(mut ui) = self.search.take() else { return };
        let out = ui.key(k, &self.sessions[self.active].items, &self.history);
        let at = ui.reveal();
        match out {
            search::Outcome::Stay => {
                self.s().reveal = at.or(self.s().reveal);
                self.search = Some(ui);
            }
            search::Outcome::Close => self.s().scroll = ui.scroll,
            search::Outcome::Go => {
                let s = self.s();
                s.reveal = at;
                (s.find_query, s.found) = (ui.query.to_lowercase(), at);
            }
            search::Outcome::Recall(i) => {
                let text = self.history[i].clone();
                self.set_input(text);
            }
        }
    }
}
