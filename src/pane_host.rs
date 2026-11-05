//! The embedded editor (input mode `editor`, ctrl+g): when it opens, who owns the keyboard, how it ends.

use super::*;

impl App {
    /// The editor to run: `editor` in config.json, else `$VISUAL`, `$EDITOR`, `vi`.
    pub(crate) fn editor(&self) -> String {
        self.editor_cmd.clone().unwrap_or_else(editor_command)
    }

    /// The editor owns the keyboard unless a prompt or panel is asking you something (full screen: always).
    pub(crate) fn pane_has_keyboard(&self) -> bool {
        self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Edit || (self.sessions[self.active].pending.is_empty() && !self.overlay_open()))
    }

    /// Start the editor on `text`. Err when it cannot (no pty): the caller falls back to the box.
    pub(crate) fn open_pane(&mut self, kind: pane::Use, text: &str) -> io::Result<()> {
        self.pane_seq += 1;
        let (w, h) = terminal::size().unwrap_or((80, 24));
        let rows = if kind == pane::Use::Edit { h.saturating_sub(1) } else { ui::pane_rows(h) };
        let pane = pane::Pane::spawn(self.pane_seq, kind, &self.editor(), text, (rows, w), self.tx.clone(), AppEvent::Pane)?;
        self.pane = Some(pane);
        self.pane_shape = u8::MAX;
        self.pane_typed = false;
        Ok(())
    }

    /// Input mode editor: the message area IS the editor. It is open whenever the box is free, and a fresh one
    /// replaces it after each send. (Claude itself still starts only when you type.)
    pub(crate) fn keep_editor_open(&mut self) {
        if !self.editor_mode {
            // Turned off: close an idle send editor (anything typed in it is lost, as with ctrl+\).
            if self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send) {
                self.pane = None;
                self.pane_closed();
            }
            self.pane_failed = false;
            return;
        }
        if self.pane.is_some() || self.pane_failed || self.overlay_open() || !self.sessions[self.active].pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.input);
        if let Err(e) = self.open_pane(pane::Use::Send, &text) {
            self.input = text;
            self.pane_failed = true;
            self.flash(format!("could not start the editor ({e}): using the plain box"));
        } else {
            self.cursor = 0;
        }
    }

    /// The editor is for typing, which is the moment to start Claude.
    pub(crate) fn warm_active(&mut self) {
        let (launch, tx) = (self.launch_for(self.active), self.tx.clone());
        self.s().warm(&launch, &tx);
    }

    pub(crate) fn pane_key(&mut self, k: KeyEvent) {
        // The one key the editor never sees: a way out if it hangs or you change your mind. (A terminal that
        // cannot tell ctrl+\ apart sends ctrl+4.)
        if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('\\' | '4')) {
            self.pane = None;
            self.pane_closed();
            let old = self.pane_old.take();
            if let Some((text, cursor)) = old {
                self.set_input(text);
                self.cursor = cursor;
            }
            return self.flash("editor closed: nothing sent");
        }
        // ctrl+z would suspend the editor, and nothing here could wake it.
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('z') {
            return self.flash("ctrl+z is not passed to the editor (it would suspend it)");
        }
        self.pane_typed = true;
        if self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send) {
            self.warm_active();
        }
        if let Some(p) = self.pane.as_mut() {
            p.key(k);
        }
    }

    pub(crate) fn on_pane(&mut self, id: u64, msg: pane::Msg) {
        let Some(p) = self.pane.as_mut().filter(|p| p.id == id) else { return };
        match msg {
            pane::Msg::Bytes(b) => {
                p.feed(&b);
                self.sync_pane_cursor();
            }
            pane::Msg::Ended => self.finish_pane(),
        }
    }

    /// Ask the terminal for the cursor shape the editor wants (vim: block in normal mode, bar in insert).
    pub(crate) fn sync_pane_cursor(&mut self) {
        let Some(shape) = self.pane.as_ref().map(|p| p.cursor_shape()) else { return };
        if shape != self.pane_shape {
            self.pane_shape = shape;
            if self.tty {
                let _ = write!(io::stdout(), "\x1b[{shape} q");
                let _ = io::stdout().flush();
            }
        }
    }

    /// The editor is gone: give the cursor back to the box.
    pub(crate) fn pane_closed(&mut self) {
        if self.tty {
            let _ = write!(io::stdout(), "\x1b[0 q");
            let _ = io::stdout().flush();
        }
        self.cursor_mode = None;
        self.vim_shape();
    }

    /// The editor exited: send (input mode editor) or return (ctrl+g) what it left in the file.
    pub(crate) fn finish_pane(&mut self) {
        let Some(p) = self.pane.take() else { return };
        let kind = p.kind;
        let out = p.finish();
        self.pane_closed();
        let old = self.pane_old.take();
        let text = match out.text {
            Ok(t) => t,
            Err(e) => return self.flash(format!("editor: could not read the message back: {e}")),
        };
        match kind {
            pane::Use::Send if !out.ok => {
                // :cq, a crash, a kill: not a send, but what was written is not thrown away.
                self.set_input(text);
                self.flash(format!("editor exited with {}: nothing sent; your text is in the box", out.status));
            }
            pane::Use::Send if text.trim().is_empty() => self.flash("empty message: nothing sent"),
            pane::Use::Send => {
                self.set_input(text);
                self.submit();
            }
            pane::Use::Edit if !out.ok => self.flash(format!("editor exited with {}; your message is as it was", out.status)),
            pane::Use::Edit => {
                let old = old.unwrap_or_default();
                if text != old.0 {
                    self.undo.push(old);
                    self.redo.clear();
                    self.set_input(text);
                    self.vim.mode = vim::Mode::Insert;
                    self.flash("message edited");
                } else {
                    self.flash("message unchanged");
                }
            }
        }
        self.vim_shape();
    }

    /// Ask the terminal for a block cursor in normal mode and a bar in insert mode.
    pub(crate) fn vim_shape(&mut self) {
        let want = self.vim.on.then_some(self.vim.mode);
        if want == self.cursor_mode {
            return;
        }
        self.cursor_mode = want;
        if self.tty {
            let shape = match want {
                None => 0,
                Some(vim::Mode::Insert) => 6,
                Some(_) => 2,
            };
            let _ = write!(io::stdout(), "\x1b[{shape} q");
            let _ = io::stdout().flush();
        }
    }
}
