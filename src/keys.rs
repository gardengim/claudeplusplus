//! Keyboard and mouse: where a key goes, the app's own chords, and the mouse.

use super::*;

impl App {
    /// With the editor open, which keys are claude++'s (translated to the key the box expects) and which are the editor's.
    /// Claimed: chords no editor needs (alt+1..9 o q p, pgup/pgdn, shift+↑↓, ctrl+home/end, shift+tab, ctrl+t o l);
    /// alt+f r d b n and alt+←/→ stand for ctrl+f r d b n and ctrl+p/n, which the editor needs for itself;
    /// and, while nothing has been typed yet, ↑ ↓ (history), ← (session list) and Esc (interrupt, rewind).
    pub(crate) fn app_key(&self, k: KeyEvent) -> Option<KeyEvent> {
        use KeyCode::*;
        let m = k.modifiers;
        let (ctrl, alt, shift) = (m.contains(KeyModifiers::CONTROL), m.contains(KeyModifiers::ALT), m.contains(KeyModifiers::SHIFT));
        let as_ctrl = |c: char| Some(KeyEvent::new(Char(c), KeyModifiers::CONTROL));
        match k.code {
            Char(c) if alt && !ctrl && matches!(c, 'f' | 'r' | 'd' | 'b' | 'n') => as_ctrl(c),
            Left if alt => as_ctrl('p'),
            Right if alt => as_ctrl('n'),
            Char('1'..='9' | 'o' | 'q' | 'p') if alt && !ctrl => Some(k),
            Char('t' | 'o' | 'l') if ctrl && !alt => Some(k),
            PageUp | PageDown | BackTab => Some(k),
            Up | Down if shift => Some(k),
            Home | End if ctrl => Some(k),
            Up | Down | Left | Esc if !self.pane_typed && m.is_empty() => Some(k),
            _ => None,
        }
    }

    pub(crate) fn on_key(&mut self, k: KeyEvent) {
        if self.term_view.is_some() {
            return self.term_view_key(k);
        }
        if self.pane_has_keyboard() {
            let send = self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send);
            let Some(app) = send.then(|| self.app_key(k)).flatten() else { return self.pane_key(k) };
            let before = self.hist_pos;
            self.on_key_app(app);
            // History put a message in the box: that is the editor's text now.
            if self.hist_pos != before && self.pane.is_some() {
                self.pane = None;
                self.pane_closed();
                self.keep_editor_open();
            }
            return;
        }
        self.on_key_app(k)
    }

    pub(crate) fn on_key_app(&mut self, k: KeyEvent) {
        let box_is_free = !self.overlay_open() && self.sessions[self.active].pending.is_empty();
        let (ctrl, alt, shift) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT), k.modifiers.contains(KeyModifiers::SHIFT));
        // Input mode editor, when the editor could not be opened up front: typing retries it.
        if self.editor_mode && !self.pane_failed && self.pane.is_none() && box_is_free && !ctrl && !alt && matches!(k.code, KeyCode::Char(_)) {
            let text = std::mem::take(&mut self.input);
            match self.open_pane(pane::Use::Send, &text) {
                Ok(()) => {
                    self.cursor = 0;
                    return self.pane_key(k);
                }
                Err(e) => {
                    self.input = text;
                    self.flash(format!("could not start the editor ({e}): typing in the box"));
                }
            }
        }
        // ctrl+z undoes; ctrl+shift+z redoes, and so does alt+z for terminals that cannot tell shift apart.
        // In vim's normal mode `u` and ctrl+r do the same.
        let in_normal = self.vim.on && self.vim.mode == vim::Mode::Normal && !self.vim.pending() && !alt;
        let redo_asked = match k.code {
            KeyCode::Char('z' | 'Z') if box_is_free && (ctrl || alt) => Some(shift || alt),
            KeyCode::Char('u') if box_is_free && in_normal && !ctrl => Some(false),
            KeyCode::Char('r') if box_is_free && in_normal && ctrl => Some(true),
            _ => None,
        };
        if let Some(redo) = redo_asked {
            let (from, to, none) = if redo { (&mut self.redo, &mut self.undo, "nothing to redo") } else { (&mut self.undo, &mut self.redo, "nothing to undo") };
            match from.pop() {
                Some(state) => {
                    to.push((std::mem::replace(&mut self.input, state.0), self.cursor));
                    self.cursor = state.1;
                }
                None => self.flash(none),
            }
            return self.vim_settle();
        }
        let before = (self.input.clone(), self.cursor);
        self.reset_undo = false;
        if !(box_is_free && self.select_in_box(k)) {
            self.handle_key(k);
        }
        if self.input != before.0 {
            self.anchor = None;
        }
        if std::mem::take(&mut self.reset_undo) {
            self.undo.clear(); // sent, or a different tab's box: nothing earlier belongs to this one
            self.redo.clear();
        } else if self.input != before.0 {
            self.redo.clear();
            // A burst of typing is one undo step; any other edit, or a pause, starts a new one.
            let typing = self.input.chars().count() == before.0.chars().count() + 1 && self.cursor == before.1 + 1;
            if !typing || self.undo.is_empty() || self.last_edit.elapsed() > Duration::from_millis(800) {
                self.undo.push(before.clone());
                self.undo.drain(..self.undo.len().saturating_sub(100));
            }
            self.last_edit = Instant::now();
        }
        if self.input != before.0 {
            self.sel = 0;
            self.drop_orphan_images("");
        }
        self.refresh_files();
        self.vim_shape();
    }

    /// The mouse: the wheel scrolls the conversation; a drag selects text inside the pane it started in and
    /// copies it when you let go; a double click takes the word, a triple click the line.
    pub(crate) fn on_mouse(&mut self, m: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};
        if self.term_view.is_some() {
            return;
        }
        let p = (m.column, m.row);
        self.mouse_log(&m);
        if matches!(m.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
            self.last_input = Instant::now(); // reading by scrolling is use
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.last_input = Instant::now();
                let (at, pos, n) = self.clicks;
                let near = pos.1 == p.1 && pos.0.abs_diff(p.0) <= 1;
                let n = if near && at.elapsed() < Duration::from_millis(450) && n < 3 { n + 1 } else { 1 };
                self.clicks = (Instant::now(), p, n);
                // In the conversation the selection is held on the text itself (see `Sel`); elsewhere on screen cells.
                let shift = m.modifiers.contains(ratatui::crossterm::event::KeyModifiers::SHIFT);
                let sel = &mut self.s().sel;
                if sel.area.contains(p.into()) && !sel.rows.is_empty() {
                    let spot = sel.at(p);
                    if !(shift && sel.a.is_some()) {
                        sel.a = spot; // shift+click keeps the start and moves the end
                    }
                    (sel.b, sel.live, sel.edge, sel.expand, sel.dirty) = (spot, true, 0, 0, true);
                    (self.drag, self.dragging) = (None, false);
                    return;
                }
                sel.clear();
                if self.input_rect.contains(p.into()) {
                    return self.press_in_box(p, shift, n);
                }
                self.region = self.panes.iter().copied().find(|r| r.contains(p.into())).unwrap_or_default();
                self.dragging = !self.region.is_empty();
                self.drag = self.dragging.then_some((p, p));
                self.snap.clear(); // the frame drawn next takes the screen as it is now
                self.sel_dirty = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.s().sel.live => {
                let sel = &mut self.s().sel;
                sel.b = sel.at(p);
                sel.edge = if p.1 < sel.area.y { -1 } else if p.1 >= sel.area.bottom() { 1 } else { 0 };
                sel.edge_at = Some(Instant::now());
                sel.dirty = true;
            }
            MouseEventKind::Up(MouseButton::Left) if self.s().sel.live => {
                let n = self.clicks.2;
                let sel = &mut self.s().sel;
                (sel.live, sel.edge, sel.dirty) = (false, 0, true);
                if sel.a == sel.b {
                    match n {
                        2 | 3 => sel.expand = n,
                        _ => sel.clear(), // a plain click only drops the selection
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.box_live => {
                self.cursor = self.input_at(p);
                self.vim_clamp();
                self.box_changed();
            }
            MouseEventKind::Up(MouseButton::Left) if self.box_live => {
                self.box_live = false;
                if self.box_selection().is_none() {
                    self.anchor = None; // a click: the cursor moved, nothing is selected
                }
                self.box_changed();
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((a, _)) = self.drag.filter(|_| self.dragging) {
                    self.drag = Some((a, p));
                    self.sel_dirty = true;
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                let Some((a, _)) = self.drag else { return };
                let region = self.region;
                self.drag = if a != p {
                    Some((a, p))
                } else {
                    match self.clicks.2 {
                        2 => self.snap.get(p.1 as usize).and_then(|row| ui::word_at(row, p.0, region)).map(|(x0, x1)| ((x0, p.1), (x1, p.1))),
                        3 => Some(((region.x, p.1), (region.right() - 1, p.1))),
                        _ => None,
                    }
                };
                self.sel_dirty = true;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.explorer.is_none() => {
                self.drag = None; // the cells it marked have moved
                let s = self.s();
                s.scroll = if m.kind == MouseEventKind::ScrollUp { s.scroll + 3 } else { s.scroll.saturating_sub(3) };
            }
            _ => {}
        }
    }

    /// `CLAUDEPP_MOUSE_LOG=<file>`: one line per mouse event as it arrived and what the selection held, to see what a
    /// terminal really sends when a selection does not behave.
    fn mouse_log(&self, m: &ratatui::crossterm::event::MouseEvent) {
        use std::io::Write as _;
        let Some(path) = std::env::var_os("CLAUDEPP_MOUSE_LOG") else { return };
        let Some(s) = self.sessions.get(self.active) else { return };
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() % 1_000_000);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(
                f,
                "{ms:06} {:?} at {},{} mods={:?} | conversation: area={:?} rows={} live={} a={:?} b={:?} | box: rect={:?} live={} | screen-drag={:?}",
                m.kind, m.column, m.row, m.modifiers, s.sel.area, s.sel.rows.len(), s.sel.live, s.sel.a, s.sel.b, self.input_rect, self.box_live, self.drag
            );
        }
    }

    /// Shift+arrows (with ctrl or alt for words), shift+home and shift+end select in the message box, copying what is
    /// selected as it grows; typing or deleting replaces it, any other key drops it. True when the key was used here.
    fn select_in_box(&mut self, k: KeyEvent) -> bool {
        use ratatui::crossterm::event::KeyModifiers as M;
        if self.vim.on && self.vim.mode == vim::Mode::Normal {
            return false;
        }
        let (words, shift) = (k.modifiers.intersects(M::CONTROL | M::ALT), k.modifiers.contains(M::SHIFT));
        let n = self.input.chars().count();
        let to = match k.code {
            KeyCode::Left if shift => Some(if words { self.word_left() } else { self.cursor.saturating_sub(1) }),
            KeyCode::Right if shift => Some(if words { self.word_right() } else { (self.cursor + 1).min(n) }),
            KeyCode::Home if shift && !k.modifiers.contains(M::CONTROL) => Some(self.line_bounds().0),
            KeyCode::End if shift && !k.modifiers.contains(M::CONTROL) => Some(self.line_bounds().1),
            _ => None,
        };
        if let Some(to) = to {
            self.anchor = Some(self.anchor.unwrap_or(self.cursor).min(n));
            self.cursor = to.min(n);
            self.box_changed();
            return true;
        }
        let Some((a, b)) = self.box_selection() else {
            self.anchor = None;
            return false;
        };
        self.anchor = None;
        let typing = matches!(k.code, KeyCode::Char(_)) && !k.modifiers.intersects(M::CONTROL | M::ALT);
        if typing {
            self.delete_range(a, b);
            false // the character goes in where the selection was
        } else if matches!(k.code, KeyCode::Backspace | KeyCode::Delete) {
            self.delete_range(a, b);
            true
        } else {
            false
        }
    }

    /// The box's selection as `[from, to)` in characters, if there is one.
    pub(crate) fn box_selection(&self) -> Option<(usize, usize)> {
        let n = self.input.chars().count();
        let a = self.anchor?.min(n);
        let c = self.cursor.min(n);
        (a != c).then(|| (a.min(c), a.max(c)))
    }

    /// The one place a selection reaches the clipboard, whatever made it (conversation, message box, a panel; mouse or
    /// shift): at most ten times a second while the button is down, always when it is let go. "copied N characters"
    /// is shown when nothing is held any more.
    pub(crate) fn flush_selection(&mut self) {
        let live = self.dragging || self.box_live || self.sessions.get(self.active).is_some_and(|s| s.sel.live);
        if live && self.copied_at.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) {
            return;
        }
        let text = self.sessions.get_mut(self.active).and_then(|s| s.sel.text.take()).or_else(|| self.pending_copy.take());
        let Some(text) = text else { return };
        if text != self.copied {
            (self.copied, self.copied_at) = (text.clone(), Some(Instant::now()));
            self.copy_text(&text);
        }
        if !live {
            self.flash(format!("copied {} characters", text.chars().count()));
        }
    }

    /// The character under a screen cell of the message box.
    fn input_at(&self, p: (u16, u16)) -> usize {
        let r = self.input_rect;
        ui::input_index(&self.input, self.input_w, (p.1.saturating_sub(r.y) as usize) + self.input_skip, p.0.saturating_sub(r.x) as usize)
    }

    /// A press in the message box: the cursor goes there; a drag from it selects; shift+click moves the end of a
    /// selection; a double click takes the word, a triple click the line.
    fn press_in_box(&mut self, p: (u16, u16), shift: bool, clicks: u8) {
        let idx = self.input_at(p);
        let chars: Vec<char> = self.input.chars().collect();
        let word = |c: &char| !c.is_whitespace();
        match clicks {
            2 if chars.get(idx).is_some_and(word) => {
                let a = (0..idx).rev().take_while(|&i| word(&chars[i])).last().unwrap_or(idx);
                let b = (idx..chars.len()).take_while(|&i| word(&chars[i])).last().map_or(idx, |i| i + 1);
                (self.anchor, self.cursor) = (Some(a), b);
            }
            3 => {
                self.cursor = idx;
                let (a, b) = self.line_bounds();
                (self.anchor, self.cursor) = (Some(a), b);
            }
            _ => {
                self.anchor = if shift { self.anchor.or(Some(self.cursor)) } else { Some(idx) };
                self.cursor = idx;
            }
        }
        self.box_live = true;
        self.vim_clamp();
        self.box_changed();
    }

    /// Vim's normal mode keeps the cursor on a character, not past the end of the line.
    fn vim_clamp(&mut self) {
        if self.vim.on && self.vim.mode == vim::Mode::Normal {
            let chars: Vec<char> = self.input.chars().collect();
            if self.cursor > 0 && chars.get(self.cursor).is_none_or(|c| *c == '\n') && chars[self.cursor - 1] != '\n' {
                self.cursor -= 1;
            }
        }
    }

    /// The message box's selection moved: offer its text for the clipboard.
    fn box_changed(&mut self) {
        if let Some((a, b)) = self.box_selection() {
            self.pending_copy = Some(self.input.chars().skip(a).take(b - a).collect());
        }
    }

    /// A pointer held past the top or bottom of the conversation keeps scrolling it, a row every 50 ms.
    pub(crate) fn autoscroll_selection(&mut self) {
        let s = self.s();
        if !s.sel.live || s.sel.edge == 0 || s.sel.edge_at.is_some_and(|t| t.elapsed() < Duration::from_millis(50)) {
            return;
        }
        s.sel.edge_at = Some(Instant::now());
        s.scroll = if s.sel.edge < 0 { s.scroll + 1 } else { s.scroll.saturating_sub(1) };
    }

    pub(crate) fn set_mouse(&mut self, on: bool) {
        self.mouse = on;
        if !on {
            self.drag = None;
        }
        if self.tty {
            // XTSHIFTESCAPE: ask the terminal to pass shift+drag to us too (xterm, Ghostty), so it selects live like a plain drag
            // instead of becoming the terminal's own selection, which our redraws disturb.
            let _ = write!(io::stdout(), "{}", mouse_modes(on));
            let _ = io::stdout().flush();
        }
    }

    pub(crate) fn handle_key(&mut self, k: KeyEvent) {
        self.handle_key_inner(k);
        self.sync_list_prefs();
    }

    pub(crate) fn handle_key_inner(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if self.front_panel().is_none() && self.dock_focus && self.dock.is_some() && self.sessions[self.active].pending.is_empty() {
            use KeyCode::*;
            if k.code == Char('x') && ctrl {
                return self.dock_key(k);
            }
            if k.code == Char('t') && ctrl {
                self.new_tab();
                self.hide_dock(); // typing now starts the new conversation
                return;
            }
            if matches!(k.code, Up | Down | PageUp | PageDown | Home | End | Enter | Right | Esc | Left | Tab) && !ctrl && !alt {
                return self.dock_key(k);
            }
            // `/` starts a filter in the panel as in the full list; once it runs, typing edits it.
            let filtering = self.dock.as_ref().is_some_and(|d| d.filtering());
            if !ctrl && !alt && (k.code == Char('/') || (filtering && matches!(k.code, Char(_) | Backspace))) {
                return self.dock_key(k);
            }
            // Typing does nothing while the list has the keys: it must not reach the message box.
            if matches!(k.code, Char(_) | Backspace | Delete) && !ctrl && !alt {
                return;
            }
            // A chord (tabs...) is the app's and hands the keys back; ctrl+c never does: it only interrupts, clears or arms quit.
            if !(k.code == Char('c') && ctrl) {
                self.hide_dock();
            }
        }
        if let Some(panel) = self.front_panel() {
            return match panel {
                Panel::Explorer => self.explorer_key(k),
                Panel::Color => self.color_key(k),
                Panel::Model => self.picker_key(k),
                Panel::Rewind => self.rewind_key(k),
                Panel::Settings => self.settings_key(k),
                Panel::Items => self.items_key(k),
                Panel::Open => self.open_key(k),
                Panel::Worktree => self.wt_key(k),
                Panel::Term => self.term_key(k),
                Panel::TabCfg => self.tabcfg_key(k),
                Panel::Instr => self.instr_key(k),
                Panel::Perms => self.perms_key(k),
                Panel::Opt => self.opt_key(k),
                Panel::Ctx => self.ctx_key(k),
                Panel::Procs => self.procs_key(k),
                Panel::Lanes => self.lanes_key(k),
                Panel::Sched => self.schedule_key(k),
                Panel::Queue => self.queue_key(k),
                Panel::Search => self.search_key(k),
                Panel::Usage => self.usage_key(k),
                Panel::Supervisor => self.sup_key(k),
            };
        }
        let rows = self.completions();
        if !rows.is_empty() {
            let sel = self.sel.min(rows.len() - 1);
            match k.code {
                KeyCode::Tab => return self.accept_completion(),
                KeyCode::Up | KeyCode::Down => return self.sel = panel::cycle(sel, rows.len(), k.code == KeyCode::Up),
                // Enter completes a partial name; once the name is complete it runs the command.
                KeyCode::Enter
                    if !alt
                        && !k.modifiers.contains(KeyModifiers::SHIFT)
                        && (rows[sel].prefix == '@' || self.input != format!("/{}", rows[sel].name)) =>
                {
                    return self.accept_completion();
                }
                KeyCode::Esc => return self.comp_off = Some(self.input.clone()),
                _ => {}
            }
        }
        if !matches!(k.code, KeyCode::Char('c')) || !ctrl {
            self.quit_armed = None;
        }
        if k.code != KeyCode::Esc {
            self.esc_armed = None;
        }

        // A permission or question panel owns the keyboard, except keys the app keeps (ctrl/alt chords for
        // tabs, interrupt..., and page keys that scroll the transcript behind it).
        let scrolling = matches!(k.code, KeyCode::PageUp | KeyCode::PageDown);
        if !ctrl && !alt && !scrolling && !self.sessions[self.active].pending.is_empty() {
            return self.prompt_key(k);
        }

        if self.sched_at.is_some() && k.code == KeyCode::Esc {
            self.sched_at = None;
            return self.flash("schedule cancelled");
        }
        // The spinner says "esc to interrupt": with nothing typed there is no insert mode to leave, so Esc does that.
        let interrupt_now = k.code == KeyCode::Esc && self.s().busy && self.input.is_empty();
        if self.vim.on && !interrupt_now && self.vim_key(k) {
            return;
        }
        match k.code {
            KeyCode::Char('f') if ctrl => self.open_search(),
            KeyCode::Char('g') if ctrl => self.edit_request = true,
            KeyCode::Char('q') if alt => self.queue_ui = Some(Default::default()),
            KeyCode::Char('c') if ctrl => {
                if self.s().busy {
                    self.s().interrupt();
                } else if !self.input.is_empty() {
                    self.set_input(String::new());
                } else if self.quit_armed.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    self.quit = true;
                } else {
                    self.quit_armed = Some(Instant::now());
                    self.flash("press ctrl+c again to quit");
                }
            }
            KeyCode::Esc if self.s().busy => self.s().interrupt(),
            // Esc Esc on an empty box goes back to an earlier message, as in Claude Code.
            KeyCode::Esc if self.input.is_empty() => {
                if self.esc_armed.take().is_some_and(|t| t.elapsed() < Duration::from_millis(800)) {
                    self.open_rewind();
                } else if !self.s().rewind_points().is_empty() {
                    self.esc_armed = Some(Instant::now());
                    self.flash("esc again to rewind");
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.close_tab_asking(),
            KeyCode::Char('t') if ctrl => self.new_tab(),
            // As in Claude Code: search what you sent before. The session list is ← (or /sessions).
            KeyCode::Char('r') if ctrl => {
                self.open_search();
                if let Some(ui) = self.search.as_mut() {
                    ui.history = true;
                    ui.refresh(&self.sessions[self.active].items, &self.history);
                }
            }
            KeyCode::Char('v') if ctrl => {
                self.paste_image("no image on the clipboard (ctrl+v pastes images; paste text with your terminal's paste)");
            }
            KeyCode::Char('n') if ctrl => self.switch(1),
            KeyCode::Char('p') if ctrl => self.switch(-1),
            KeyCode::Char(c @ '1'..='9') if alt => {
                let i = c as usize - '1' as usize;
                if i < self.sessions.len() {
                    self.goto(i);
                }
            }
            KeyCode::Char('o') if ctrl => {
                self.expanded = !self.expanded;
                for s in &mut self.sessions {
                    s.items.iter_mut().for_each(|i| (i.cache, i.flipped) = (None, false));
                }
            }
            KeyCode::Char('o') if alt => {
                if !self.s().flip_call(None) {
                    self.flash("no tool call to fold");
                }
            }
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => self.insert("\n"),
            KeyCode::Enter => self.submit(),
            KeyCode::BackTab => {
                let modes = tabcfg::modes();
                let next = modes[(modes.iter().position(|m| *m == self.s().mode).map_or(0, |i| i + 1)) % modes.len()];
                self.set_mode(next);
            }
            KeyCode::Char('a') if ctrl => self.cursor = self.line_bounds().0,
            KeyCode::Char('e') if ctrl => self.cursor = self.line_bounds().1,
            // ctrl+u / ctrl+k cut to the start / end of the line; at its edge they take the line break with them.
            KeyCode::Char('u') if ctrl => {
                let a = self.line_bounds().0;
                self.delete_range(a.saturating_sub((a == self.cursor) as usize), self.cursor)
            }
            KeyCode::Char('w') if ctrl => self.delete_range(self.word_left(), self.cursor),
            KeyCode::Backspace if alt => self.delete_range(self.word_left(), self.cursor),
            KeyCode::Char('d') if alt => self.delete_range(self.cursor, self.word_right()),
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
            KeyCode::Char('k') if ctrl => {
                let b = self.line_bounds().1;
                self.delete_range(self.cursor, if b == self.cursor { (b + 1).min(self.input.chars().count()) } else { b })
            }
            KeyCode::Char('p') if alt => self.open_model_picker(), // as in Claude Code
            KeyCode::Char('b') if ctrl => {
                let said = self.s().background_shell();
                self.flash(said);
            }
            KeyCode::Char('l') if ctrl => self.clear_request = true,
            // An unbound alt+letter is a shortcut that does not exist here, not text to type.
            KeyCode::Char(c) if !ctrl && !(alt && c.is_ascii_alphabetic()) => {
                self.insert(c.encode_utf8(&mut [0; 4]));
                self.warm();
            }
            KeyCode::Backspace if self.cursor > 0 && self.token_len(true).is_some() => {
                let len = self.token_len(true).unwrap();
                let (a, b) = (self.byte_at(self.cursor - len), self.byte_at(self.cursor));
                self.input.replace_range(a..b, "");
                self.cursor -= len;
            }
            KeyCode::Delete if self.token_len(false).is_some() => {
                let len = self.token_len(false).unwrap();
                let (a, b) = (self.byte_at(self.cursor), self.byte_at(self.cursor + len));
                self.input.replace_range(a..b, "");
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_at(self.cursor);
                self.input.remove(at);
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let at = self.byte_at(self.cursor);
                self.input.remove(at);
            }
            KeyCode::Left if alt || ctrl => self.cursor = self.word_left(),
            KeyCode::Right if alt || ctrl => self.cursor = self.word_right(),
            // As in Claude Code: ← with nothing to the left of the cursor goes to the session list.
            KeyCode::Left if self.cursor == 0 && !ctrl && !alt => {
                if self.dock_mode { self.show_dock(true) } else { self.open_explorer() }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home if ctrl => self.s().scroll = usize::MAX / 2,
            KeyCode::End if ctrl => self.s().scroll = 0,
            KeyCode::Home => self.cursor = self.line_bounds().0,
            KeyCode::End => self.cursor = self.line_bounds().1,
            KeyCode::Up if k.modifiers.contains(KeyModifiers::SHIFT) => self.s().scroll += 1,
            KeyCode::Down if k.modifiers.contains(KeyModifiers::SHIFT) => {
                self.s().scroll = self.s().scroll.saturating_sub(1)
            }
            KeyCode::PageUp => self.s().scroll += self.s().view_h.saturating_sub(2).max(1),
            KeyCode::PageDown => self.s().scroll = self.s().scroll.saturating_sub(self.s().view_h.saturating_sub(2).max(1)),
            // ↑/↓ move through a message that is more than one row; from its first / last row they walk history.
            KeyCode::Up if !self.move_row(-1) => self.history_step(-1),
            KeyCode::Down if !self.move_row(1) => self.history_step(1),
            _ => {}
        }
    }

    /// Route a key to the panel at the front of this tab's prompt queue.
    pub(crate) fn prompt_key(&mut self, k: KeyEvent) {
        enum Act {
            Nothing,
            Choose(Choice),
            Answers(Vec<(String, String)>),
        }
        let act = {
            let Some(p) = self.sessions[self.active].pending.front_mut() else { return };
            if p.is_question() {
                let qs = p.questions().to_vec();
                match p.ui.question_key(&qs, k) {
                    prompt::Outcome::Stay => Act::Nothing,
                    prompt::Outcome::Decline => Act::Choose(Choice::Deny),
                    prompt::Outcome::Submit(a) => Act::Answers(a),
                }
            } else {
                let has_always = p.suggestions.as_array().is_some_and(|a| !a.is_empty());
                p.ui.permission_key(has_always, k).map_or(Act::Nothing, Act::Choose)
            }
        };
        match act {
            Act::Nothing => {}
            Act::Choose(c) => self.s().answer(c),
            Act::Answers(a) => self.s().submit_answers(a),
        }
    }
}

/// The panels that take the keys, in the order they win: the one `front_panel` names is the one that is drawn and answers.
impl App {
    fn explorer_key(&mut self, k: KeyEvent) {
        if let Some(ex) = self.explorer.as_mut() {
            match ex.key(k) {
                explorer::Outcome::Stay => {}
                explorer::Outcome::Close => {
                    let picked = ex.selected();
                    self.explorer = None;
                    if self.dock_mode {
                        self.show_dock(true); // Esc from the full screen steps back to the docked list, on the same row
                        if let (Some((id, _)), Some(d)) = (picked, self.dock.as_mut()) {
                            d.select(&id);
                        }
                    }
                }
                other => {
                    let close = !matches!(other, explorer::Outcome::Refused(_) | explorer::Outcome::Interrupt(_));
                    if close {
                        self.explorer = None;
                    }
                    self.explorer_outcome(other);
                }
            }
        }
    }

    fn ctx_key(&mut self, k: KeyEvent) {
        if let Some(mut ui) = self.ctx_ui.take() {
            let ctx = self.sessions[self.active].usage.ctx.clone();
            match ui.key(k, ctx.as_ref().map_or(0, context::count)) {
                context::Out::Keep => self.ctx_ui = Some(ui),
                context::Out::Close => {}
                context::Out::Refresh => {
                    self.ctx_ui = Some(ui);
                    self.ask_usage();
                }
                context::Out::Act => match ctx.and_then(|c| context::action(&c, ui.sel)) {
                    Some((cmd, _)) => {
                        self.input = cmd.into();
                        self.cursor = self.input.chars().count();
                        self.submit();
                    }
                    None => self.ctx_ui = Some(ui),
                },
            }
        }
    }

    fn procs_key(&mut self, k: KeyEvent) {
        if let Some(mut ui) = self.procs_ui.take()
            && ui.key(k)
        {
            let reload = std::mem::take(&mut ui.reload);
            self.procs_ui = Some(ui);
            if reload {
                self.read_procs();
            }
        }
    }

    fn lanes_key(&mut self, k: KeyEvent) {
        if let Some(mut ui) = self.lanes.take() {
            let all = lanes::collect(&self.sessions[self.active].items, &self.sessions[self.active].shells, self.cwd());
            let keep = ui.key(k, all.len());
            if let Some(id) = ui.stop.take().and_then(|i| all.get(i)?.stop.clone()) {
                self.s().control("stop_task", json!({"task_id": id}));
                self.flash("stopping the command");
            }
            if keep {
                self.lanes = Some(ui);
            }
        }
    }

    fn usage_key(&mut self, k: KeyEvent) {
        if self.usage_open {
            match k.code {
                KeyCode::Esc => self.usage_open = false,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.usage_open = false,
                KeyCode::Char('r') => self.open_usage(),
                _ => {}
            }
        }
    }
}

/// The escapes that turn mouse reporting on or off. Terminals such as Ghostty keep only the LAST of the tracking
/// modes 1000 (press/release) and 1002 (and motion while a button is held), so 1002 goes after 1000: the other way
/// round the terminal sends no drag at all and nothing can be selected by dragging. 1006 gives coordinates past
/// column 223; the last one asks the terminal to pass shift+drag on (XTSHIFTESCAPE).
pub(crate) fn mouse_modes(on: bool) -> String {
    if on { "\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[>1s".into() } else { "\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[>0s".into() }
}
