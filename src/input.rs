//! The message box: typing and editing, pasted text and attachments, completion, history, vim.

use super::*;

impl App {
    pub(crate) fn on_paste(&mut self, p: String) {
        if let Some(t) = &self.term_view {
            return t.paste(&p);
        }
        if self.pane_has_keyboard() {
            self.pane_typed = true;
            if self.pane.as_ref().is_some_and(|p| p.kind == pane::Use::Send) {
                self.warm_active();
            }
            return self.pane.as_mut().map_or((), |e| e.paste(&p));
        }
        if self.overlay_open() {
            return;
        }
        // Input mode editor: the paste is what the editor opens with (keys sent before it is ready could be commands).
        if self.editor_mode && !self.pane_failed && self.pane.is_none() && self.sessions[self.active].pending.is_empty() {
            let text = format!("{}{}", std::mem::take(&mut self.input), p.replace("\r\n", "\n").replace('\r', "\n"));
            match self.open_pane(pane::Use::Send, &text) {
                Ok(()) => return self.cursor = 0,
                Err(e) => {
                    self.input = text;
                    self.cursor = self.input.chars().count();
                    return self.flash(format!("could not start the editor ({e}): pasted into the box"));
                }
            }
        }
        // A question takes pasted text as its typed answer; a permission prompt only as the reason on its
        // "No" row (a pasted "y" must never approve anything).
        if let Some(pending) = self.sessions[self.active].pending.front_mut() {
            if pending.is_question() {
                let qs = pending.questions().to_vec();
                pending.ui.question_paste(&qs, &p);
            } else {
                pending.ui.paste_feedback(&p, pending.suggestions.as_array().is_some_and(|a| !a.is_empty()));
            }
            return;
        }
        if p.is_empty() {
            // Terminals send an empty paste when the clipboard holds an image and no text.
            self.paste_image("nothing to paste");
            return;
        }
        let p = p.replace("\r\n", "\n").replace('\r', "\n");
        // Dragging an image into the terminal pastes its path: attach it instead.
        let path = p.trim().trim_matches(|c| c == '\'' || c == '"').replace("\\ ", " ");
        if !path.contains('\n') && image::media_type(&path).is_some() && std::path::Path::new(&path).is_file() {
            self.attach(&path);
            return;
        }
        self.insert(&p);
        self.warm();
        self.refresh_files();
    }

    /// Length of an `[Image #n]` token ending (`back`) or starting at the cursor, so it is deleted whole.
    pub(crate) fn token_len(&self, back: bool) -> Option<usize> {
        let chars: Vec<char> = self.input.chars().collect();
        if back { image::token_ending_at(&chars, self.cursor) } else { image::token_starting_at(&chars, self.cursor) }
    }

    /// Read the clipboard's image on a thread (osascript takes ~100–200 ms); it is attached when it arrives.
    pub(crate) fn paste_image(&mut self, miss: &'static str) {
        let (sid, tx) = (self.sessions[self.active].id, self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(AppEvent::Clipboard(sid, image::clipboard_png(), miss));
        });
    }

    pub(crate) fn attach(&mut self, path: &str) {
        let Some(mt) = image::media_type(path) else {
            return self.flash("only png/jpeg/gif/webp images can be attached");
        };
        match std::fs::read(path) {
            Ok(b) => {
                let name = std::path::Path::new(path).file_name().unwrap().to_string_lossy().into_owned();
                self.attach_bytes(name, b, mt);
            }
            Err(e) => self.flash(format!("{path}: {e}")),
        }
    }

    /// Queue an image for the next message. Over the API's 5 MB limit it is re-encoded smaller first.
    pub(crate) fn attach_bytes(&mut self, name: String, bytes: Vec<u8>, media: &str) {
        const LIMIT: usize = 5 * 1024 * 1024;
        let (bytes, media) = if bytes.len() > LIMIT {
            match image::shrink(&bytes).filter(|b| b.len() <= LIMIT) {
                Some(b) => (b, "image/png"),
                None => return self.flash("image is over the 5 MB API limit and could not be shrunk"),
            }
        } else {
            (bytes, media)
        };
        let block = json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": image::base64(&bytes)}});
        let png = if image::enabled() { image::prepare(&bytes, media) } else { None };
        let width = ratatui::crossterm::terminal::size().map_or(80, |s| s.0 as u32);
        let preview = png.as_deref().and_then(|p| image::upload(p, width.saturating_sub(6).min(36), 5));
        let n = self.s().attachments.iter().map(|a| a.n).max().unwrap_or(0) + 1;
        self.s().attachments.push(session::Attachment { n, name: name.clone(), block, png, preview });
        // The picture lives in the text as `[Image #n]`, on its own line, wherever the cursor was.
        let chars: Vec<char> = self.input.chars().collect();
        let before = self.cursor > 0 && chars.get(self.cursor - 1) != Some(&'\n');
        let after = chars.get(self.cursor) != Some(&'\n');
        let token = format!("{}{}{}", if before { "\n" } else { "" }, image::token(n), if after { "\n" } else { "" });
        self.insert(&token);
        self.flash(format!("attached {name} as {}", image::token(n)));
    }

    pub(crate) fn insert(&mut self, t: &str) {
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, t);
        self.cursor += t.chars().count();
    }

    /// Start of the word before the cursor (whitespace, newlines included, separates words).
    pub(crate) fn word_left(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// End of the word after the cursor.
    pub(crate) fn word_right(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Remove the characters between two positions; the cursor ends at the lower one.
    pub(crate) fn delete_range(&mut self, from: usize, to: usize) {
        let (a, b) = (self.byte_at(from.min(to)), self.byte_at(from.max(to)));
        self.input.replace_range(a..b, "");
        self.cursor = from.min(to);
    }

    pub(crate) fn byte_at(&self, char_idx: usize) -> usize {
        self.input.char_indices().nth(char_idx).map_or(self.input.len(), |(b, _)| b)
    }

    pub(crate) fn set_input(&mut self, t: String) {
        self.cursor = t.chars().count();
        self.input = t;
    }

    /// Free attachments whose `[Image #n]` token is in neither the input, `also`, nor anything ctrl+z can bring back.
    pub(crate) fn drop_orphan_images(&mut self, also: &str) {
        if self.sessions[self.active].attachments.is_empty() {
            return;
        }
        let mut present = image::tokens_in(&self.input);
        present.extend(image::tokens_in(also));
        present.extend(self.undo.iter().chain(&self.redo).flat_map(|(t, _)| image::tokens_in(t)));
        self.s().attachments.retain(|a| {
            let keep = present.contains(&a.n);
            if !keep {
                a.preview.iter().for_each(|p| image::delete(p.id));
            }
            keep
        });
    }

    /// The `@word` being typed at the cursor: (char index of the `@`, text after it).
    pub(crate) fn at_token(&self) -> Option<(usize, String)> {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor.min(chars.len());
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        (chars.get(start) == Some(&'@') && start < self.cursor).then(|| (start, chars[start + 1..self.cursor].iter().collect()))
    }

    /// Keep the `@` popup's rows in step with the word under the cursor.
    pub(crate) fn refresh_files(&mut self) {
        let tok = if self.overlay_open() { None } else { self.at_token() };
        let Some((_, q)) = tok else {
            self.file_rows.clear();
            self.file_key = None;
            return;
        };
        if self.file_key.as_deref() == Some(q.as_str()) {
            return;
        }
        if self.file_key.is_none() && self.files_at.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            // Listing walks the tree outside a git repo (over a second in a home folder): built off the
            // UI thread, the popup ranks the previous list until it arrives.
            self.files_at = Some(Instant::now());
            let (cwd, tx) = (self.cwd().to_string(), self.tx.clone());
            if cfg!(test) {
                self.files = files::list(&cwd); // tests read their event channel themselves
            } else {
                std::thread::spawn(move || {
                    let list = files::list(&cwd);
                    let _ = tx.send(AppEvent::Files(cwd, list));
                });
            }
        }
        self.file_rows = files::rank(&self.files, &q, 30)
            .into_iter()
            .map(|name| {
                let desc = if name.ends_with('/') { "folder" } else if image::media_type(&name).is_some() { "image" } else { "" };
                Completion { prefix: '@', desc: desc.into(), name, hint: String::new() }
            })
            .collect();
        self.file_key = Some(q);
    }

    /// Rows for the popup: `/commands` when the whole input is one slash word, else `@files`.
    pub fn completions(&self) -> Vec<Completion> {
        if self.comp_off.as_deref() == Some(self.input.as_str()) || self.sessions[self.active].pending.front().is_some() {
            return vec![];
        }
        let Some(q) = self.input.strip_prefix('/').filter(|q| !q.contains(char::is_whitespace)) else {
            return self.file_rows.clone();
        };
        let theirs = &self.sessions[self.active].commands;
        let theirs = if theirs.is_empty() { &self.known_commands } else { theirs };
        let q = q.to_lowercase();
        // Sorted by (name only contains the query, not one of ours, name): prefix hits first, ours before Claude Code's.
        let mut rows: Vec<(bool, bool, Completion)> = LOCAL_COMMANDS
            .iter()
            .map(|(n, h, d)| (true, Completion { prefix: '/', name: n.to_string(), hint: h.to_string(), desc: theme::keys(d) }))
            .chain(
                theirs
                    .iter()
                    .filter(|c| !LOCAL_COMMANDS.iter().any(|l| l.0 == c.name))
                    .map(|c| (false, Completion { prefix: '/', name: c.name.clone(), hint: c.hint.clone(), desc: c.desc.clone() })),
            )
            .filter_map(|(ours, c)| {
                let n = c.name.to_lowercase();
                if n.starts_with(&q) {
                    Some((false, !ours, c))
                } else {
                    n.contains(&q).then_some((true, !ours, c))
                }
            })
            .collect();
        rows.sort_by(|a, b| (a.0, a.1, &a.2.name).cmp(&(b.0, b.1, &b.2.name)));
        rows.into_iter().map(|r| r.2).collect()
    }

    pub(crate) fn accept_completion(&mut self) {
        let rows = self.completions();
        let Some(c) = rows.get(self.sel.min(rows.len().saturating_sub(1))) else { return };
        if c.prefix == '/' {
            return self.set_input(format!("/{} ", c.name));
        }
        // A folder keeps the popup open for the next level; a file ends the word.
        let Some((start, _)) = self.at_token() else { return };
        let insert = format!("@{}{}", c.name, if c.name.ends_with('/') { "" } else { " " });
        let (a, b) = (self.byte_at(start), self.byte_at(self.cursor));
        self.input.replace_range(a..b, &insert);
        self.cursor = start + insert.chars().count();
    }

    /// A key for vim's modes. True when it was used up; false hands it on to the ordinary keys.
    pub(crate) fn vim_key(&mut self, k: KeyEvent) -> bool {
        let mut t: Vec<char> = self.input.chars().collect();
        let mut cur = self.cursor.min(t.len());
        let out = self.vim.key(k, &mut t, &mut cur);
        let text: String = t.iter().collect();
        if text != self.input {
            self.input = text;
        }
        self.cursor = cur;
        match out {
            vim::Out::Pass => return false,
            vim::Out::Row(d) => {
                (0..d.unsigned_abs()).for_each(|_| _ = self.move_row(d.signum()));
                self.vim_settle();
            }
            vim::Out::Search => self.open_search(),
            vim::Out::Done | vim::Out::Undo | vim::Out::Redo => {}
        }
        true
    }

    /// In vim's normal and visual modes the cursor sits on a character, not after the last one.
    pub(crate) fn vim_settle(&mut self) {
        if self.vim.on {
            let t: Vec<char> = self.input.chars().collect();
            self.vim.settle(&t, &mut self.cursor);
        }
    }

    /// The message box's current line (between line breaks) as (start, end) char positions.
    pub(crate) fn line_bounds(&self) -> (usize, usize) {
        let chars: Vec<char> = self.input.chars().collect();
        let a = chars[..self.cursor.min(chars.len())].iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
        (a, chars[a..].iter().position(|c| *c == '\n').map_or(chars.len(), |i| a + i))
    }

    /// Move the cursor one row up or down in the box, keeping its column. False when there is no such row
    /// and the key is history's (↓ with no history shown just goes to the end).
    pub(crate) fn move_row(&mut self, d: isize) -> bool {
        let (rows, (col, row)) = ui::input_layout(&self.input, self.cursor, self.input_w.max(1));
        let to = row as isize + d;
        if to < 0 {
            return false;
        }
        let to = to as usize;
        if to >= rows.len() {
            if self.hist_pos.is_some() {
                return false;
            }
            self.cursor = self.input.chars().count();
            return true;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = 0;
        for r in &rows[..to] {
            start += r.chars().count();
            start += (chars.get(start) == Some(&'\n')) as usize;
        }
        let (mut w, mut n) = (0, 0);
        for ch in rows[to].chars() {
            w += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w > col {
                break;
            }
            n += 1;
        }
        self.cursor = start + n;
        true
    }

    pub(crate) fn history_step(&mut self, d: isize) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        let pos = match (self.hist_pos, d) {
            (None, -1) => {
                self.hist_draft = self.input.clone();
                Some(n - 1)
            }
            (None, _) => None,
            (Some(0), -1) => Some(0),
            (Some(p), -1) => Some(p - 1),
            (Some(p), _) if p + 1 < n => Some(p + 1),
            (Some(_), _) => None,
        };
        self.hist_pos = pos;
        let text = match pos {
            Some(p) => self.history[p].clone(),
            None => std::mem::take(&mut self.hist_draft),
        };
        self.set_input(text);
    }

    /// Image files named as `@path` in a message: Claude Code reads them, and we show them inline.
    pub(crate) fn mentioned_images(&self, text: &str) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = Vec::new();
        for word in text.split_whitespace().filter_map(|w| w.strip_prefix('@')) {
            let word = word.trim_end_matches([',', '.', ';', ':', ')', '!', '?']);
            if image::media_type(word).is_none() {
                continue;
            }
            let p = PathBuf::from(self.cwd()).join(word);
            if p.is_file() && !found.contains(&p) {
                found.push(p);
            }
        }
        found
    }
}
