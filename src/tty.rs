//! The terminal: raw mode and its restore, handing it to an external editor, and presenting a frame.

use super::*;

pub(crate) static ENHANCED: AtomicBool = AtomicBool::new(false);

/// Set while an external editor owns the terminal: the reader thread must leave its keys alone.
pub(crate) static EDITING: AtomicBool = AtomicBool::new(false);

/// `text` edited in `editor` (a shell command, so `code -w` works) through a private temp file; the editor's
/// last line break is not part of the message. Err when the editor could not run or failed.
pub(crate) fn edit_text(editor: &str, text: &str) -> io::Result<String> {
    let path = pane::private_temp("claudepp-message", text)?;
    let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status();
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    let status = status?;
    if !status.success() {
        return Err(io::Error::other(format!("{editor} {status}")));
    }
    let edited = edited?;
    Ok(edited.strip_suffix('\n').unwrap_or(&edited).to_string())
}

/// The editor to use: `$VISUAL`, else `$EDITOR`, else `vi`.
pub(crate) fn editor_command() -> String {
    ["VISUAL", "EDITOR"].iter().filter_map(|k| std::env::var(k).ok()).find(|v| !v.trim().is_empty()).unwrap_or_else(|| "vi".into())
}

pub(crate) type Term = (ratatui::Terminal<ratatui::backend::CrosstermBackend<screen::Frames<io::Stdout>>>, screen::Frames<io::Stdout>);

/// Send the frame ratatui just drew (see `screen.rs`: the writer holds it back until now).
pub(crate) fn present(term: &Term) {
    let _ = term.1.present();
}

/// Everything claude++ may have switched on in the terminal's mouse and cursor handling, switched off again: mouse
/// reporting, XTSHIFTESCAPE (left on it would keep shift+drag from selecting in the shell afterwards), the saved
/// alternate-scroll mode, and the cursor shape.
pub(crate) const RESET: &str = "\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[>0s\x1b[?1007r\x1b[0 q";

/// Give the terminal to a program (the editor) for the length of `f`, then take it back and repaint.
pub(crate) fn with_terminal_lent<T>(term: &mut Term, app: &mut App, f: impl FnOnce() -> T) -> T {
    EDITING.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(120)); // the reader thread is out of its read by now
    let mut out = io::stdout();
    let _ = write!(out, "{RESET}");
    if ENHANCED.load(Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
    let res = f();
    // Not `ratatui::init()`: that installs another panic hook each time, one that restores the terminal
    // on a worker thread's panic too.
    let _ = terminal::enable_raw_mode();
    let _ = execute!(io::stdout(), terminal::EnterAlternateScreen);
    let mut out = io::stdout();
    let _ = execute!(out, EnableBracketedPaste, EnableFocusChange);
    alt_scroll_off();
    if app.mouse {
        app.set_mouse(true);
    }
    if ENHANCED.load(Ordering::Relaxed) {
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    app.cursor_mode = None;
    // The loop did not run while the editor had the terminal: you were here, and the focus events were off.
    (app.last_input, app.focus) = (Instant::now(), None);
    let _ = term.0.clear();
    present(term);
    EDITING.store(false, Ordering::Relaxed);
    res
}

/// ctrl+g: hand the terminal to the editor with the message in it, take the result back into the box.
pub(crate) fn edit_in_editor(term: &mut Term, app: &mut App) {
    let editor = app.editor();
    let old = (app.input.clone(), app.cursor);
    // Preferably inside claude++ (the conversation keeps updating); the terminal is only lent when that cannot start.
    match app.open_pane(pane::Use::Edit, &old.0) {
        Ok(()) => return app.pane_old = Some(old),
        Err(e) => app.flash(format!("embedded editor unavailable ({e}): opening it the plain way")),
    }
    let res = with_terminal_lent(term, app, || edit_text(&editor, &old.0));
    match res {
        Ok(text) if text != old.0 => {
            app.undo.push(old);
            app.redo.clear();
            app.set_input(text);
            app.vim.mode = vim::Mode::Insert;
            app.flash("message edited");
        }
        Ok(_) => app.flash("message unchanged"),
        Err(e) => app.flash(format!("editor: {e}; your message is as it was (set $VISUAL or $EDITOR)")),
    }
    app.vim_shape();
}

/// `/memory`: edit a CLAUDE.md in place. Claude Code watches it, so the next turn reads the change.
pub(crate) fn edit_file_in_editor(term: &mut Term, app: &mut App, path: &std::path::Path) {
    let editor = app.editor();
    let status = with_terminal_lent(term, app, || std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path).status());
    match status {
        Ok(s) if s.success() => app.flash(format!("saved {}", path.display())),
        Ok(s) => app.flash(format!("editor: {editor} {s}")),
        Err(e) => app.flash(format!("editor: {e} (set $VISUAL or $EDITOR)")),
    }
}

/// The mouse stays the terminal's, so selecting works as everywhere. Many terminals turn the wheel into ↑/↓ on the
/// alternate screen (alternate scroll, mode 1007), which would walk the history: save the mode, switch it off,
/// and `restore_terminal` puts it back.
pub(crate) fn alt_scroll_off() {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[?1007s\x1b[?1007l");
    let _ = out.flush();
}

/// Put the terminal back the way we found it: mouse reporting off (claude++ never turns it on; this clears a stale state), paste and key modes reset, images
/// deleted, raw mode and the alternate screen left. Safe to call twice (the panic hook and normal exit).
pub(crate) fn restore_terminal() {
    let mut out = io::stdout();
    let _ = write!(out, "{RESET}");
    if ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste, DisableFocusChange);
    image::clear_all();
    let _ = write!(out, "\x1b[23;0t"); // give the terminal its own title back
    ratatui::restore();
}

/// The one thread that reads the terminal and turns what it sees into events.
pub(crate) fn spawn_reader(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let (mut last_input, mut last_check) = (Instant::now(), Instant::now());
        loop {
            // Polling, not blocking in read: an editor started with ctrl+g must get the keys, not this thread.
            if EDITING.load(Ordering::Relaxed) || !event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if EDITING.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                // The terminal may switch light/dark under us (macOS appearance at sunset). Its notification of
                // that would wedge crossterm's parser, so ask again instead: from this thread, the only reader of
                // the terminal, and only while nobody types (a key arriving during the round trip would be
                // read as part of the reply), and only from a terminal that answered at startup.
                // ponytail: a key typed in the query's ~ms window is lost; a raw-input reader could re-inject it.
                if theme::follows_terminal() && last_input.elapsed() > Duration::from_secs(3) && last_check.elapsed() > Duration::from_secs(3) {
                    last_check = Instant::now();
                    if let Some(was) = theme::detected()
                        && let Some(now) = theme::query_light_background()
                        && now != was
                        && tx.send(AppEvent::Theme(now)).is_err()
                    {
                        break;
                    }
                }
                continue;
            }
            last_input = Instant::now();
            let Ok(e) = event::read() else { break };
            if tx.send(AppEvent::Term(e)).is_err() {
                break;
            }
        }
    });
}
