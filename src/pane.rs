//! An editor running inside claude++: a pty, the `vt100` terminal emulator, and the keys in between.
//!
//! Two uses of one component: in the message box (input mode "editor": the editor is the box, quitting it sends
//! the message) and full screen (ctrl+g: quitting puts the text back in the box). Either way the editor edits a
//! private temp file and the text is read back when the editor exits, so claude++ never has to understand the
//! editor, only draw it and pass keys through.

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Sender, channel};
use std::thread;

/// What happens when the editor exits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Use {
    /// Input mode "editor": the text is sent as the message.
    Send,
    /// ctrl+g: the text goes back into the message box.
    Edit,
}

pub enum Msg {
    Bytes(Vec<u8>),
    /// The pty closed: the editor (and anything it started on that terminal) is gone.
    Ended,
}

/// How it ended: the text in the file, and whether the editor said it was fine (exit status 0; `:cq` is not).
pub struct Outcome {
    pub text: io::Result<String>,
    pub ok: bool,
    pub status: String,
}

/// Answers to the questions editors ask a terminal at start-up (device attributes, cursor position), which the
/// emulator does not answer itself. Without them some editors wait for a timeout before they paint.
#[derive(Default)]
struct Replies(Vec<u8>);

impl vt100::Callbacks for Replies {
    fn unhandled_csi(&mut self, screen: &mut vt100::Screen, i1: Option<u8>, _i2: Option<u8>, params: &[&[u16]], c: char) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, p0) {
            (None, 'c', 0) => self.0.extend(b"\x1b[?62;c"),
            (Some(b'>'), 'c', 0) => self.0.extend(b"\x1b[>0;0;0c"),
            (None, 'n', 5) => self.0.extend(b"\x1b[0n"),
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.0.extend(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            }
            _ => {}
        }
    }
}

pub struct Pane {
    pub id: u64,
    pub kind: Use,
    /// The editor's command as the user wrote it, for the title.
    pub editor: String,
    child: Option<Child>,
    writes: Sender<Vec<u8>>,
    parser: vt100::Parser<Replies>,
    path: PathBuf,
    size: (u16, u16),
    /// Keys typed before the editor painted anything: held, since it is not reading yet.
    held: Vec<u8>,
    painted: bool,
    master: OwnedFd,
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).unwrap_or(NonZeroU16::MIN)
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: 0, ws_ypixel: 0 }
}

/// A new file in the temp folder, readable by you alone, holding `text`. The name is unique within this process.
pub fn private_temp(prefix: &str, text: &str) -> io::Result<std::path::PathBuf> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{}.md", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    f.write_all(text.as_bytes())?;
    Ok(path)
}

impl Pane {
    /// Start `editor` (a shell command, so `code -w` works) on a temp file holding `text`.
    pub fn spawn<E: Send + 'static>(
        id: u64,
        kind: Use,
        editor: &str,
        text: &str,
        (rows, cols): (u16, u16),
        tx: Sender<E>,
        wrap: impl Fn(u64, Msg) -> E + Send + 'static,
    ) -> io::Result<Pane> {
        let path = private_temp("claudepp-pane", text)?;
        let started = Self::start(editor, &path, (rows, cols));
        let (child, master) = match started {
            Ok(x) => x,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        };
        let mut reader = File::from(master.try_clone()?);
        thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EIO is how a closed pty reads on macOS
                    Ok(n) => {
                        if tx.send(wrap(id, Msg::Bytes(buf[..n].to_vec()))).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(wrap(id, Msg::Ended));
        });
        // Writes go through a thread: a pasted megabyte, or an editor that stops reading, must not block the UI.
        let mut writer = File::from(master.try_clone()?);
        let (writes, queue) = channel::<Vec<u8>>();
        thread::spawn(move || {
            for bytes in queue {
                if writer.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        Ok(Pane {
            id,
            kind,
            editor: editor.to_string(),
            child: Some(child),
            writes,
            parser: vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 0, Replies::default()),
            path,
            size: (rows, cols),
            held: vec![],
            painted: false,
            master,
        })
    }

    fn start(editor: &str, path: &std::path::Path, (rows, cols): (u16, u16)) -> io::Result<(Child, OwnedFd)> {
        let (mut m, mut s) = (-1, -1);
        let mut ws = winsize(rows, cols);
        // SAFETY: plain libc call with valid out-pointers; the fds are wrapped right after.
        if unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty just returned these descriptors and nothing else owns them.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        for fd in [&master, &slave] {
            // SAFETY: fcntl on a descriptor we own. Not inherited by the editor except as its stdio.
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path).env("TERM", "xterm-256color").env("COLORTERM", "truecolor");
        cmd.stdin(Stdio::from(slave.try_clone()?)).stdout(Stdio::from(slave.try_clone()?)).stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe; this makes the pty the editor's controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        Ok((child, master))
    }

    /// Output from the editor: draw it, answer what it asks, and release keys typed before it was ready.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        let replies = std::mem::take(&mut self.parser.callbacks_mut().0);
        if !replies.is_empty() {
            let _ = self.writes.send(replies);
        }
        if !self.painted {
            self.painted = true;
            if !self.held.is_empty() {
                let _ = self.writes.send(std::mem::take(&mut self.held));
            }
        }
    }

    fn write(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if self.painted {
            let _ = self.writes.send(bytes);
        } else {
            self.held.extend(bytes);
        }
    }

    pub fn key(&mut self, k: KeyEvent) {
        let app_cursor = self.parser.screen().application_cursor();
        self.write(encode(k, app_cursor));
    }

    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n");
        if self.parser.screen().bracketed_paste() {
            self.write(format!("\x1b[200~{text}\x1b[201~").into_bytes());
        } else {
            self.write(text.replace('\n', "\r").into_bytes());
        }
    }

    /// Match the editor's terminal to the area it is drawn in.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let ws = winsize(rows, cols);
        // SAFETY: TIOCSWINSZ on the master we own; the kernel signals the editor with SIGWINCH.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        self.parser.set_size(nz(rows), nz(cols));
    }

    /// The editor exited (its pty may stay open if it left something running on it).
    pub fn exited(&mut self) -> bool {
        self.child.as_mut().is_none_or(|c| matches!(c.try_wait(), Ok(Some(_)) | Err(_)))
    }

    /// DECSCUSR number for the cursor shape the editor asked for (0 = the terminal's own).
    pub fn cursor_shape(&self) -> u8 {
        use vt100::CursorStyle::*;
        match self.parser.screen().cursor_style() {
            Default => 0,
            BlinkBlock => 1,
            SteadyBlock => 2,
            BlinkUnderline => 3,
            SteadyUnderline => 4,
            BlinkVerticalBar => 5,
            SteadyVerticalBar => 6,
        }
    }

    /// Draw the editor's screen into `area`; returns where its cursor is, if it shows one.
    pub fn render(&self, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
        draw_screen(self.parser.screen(), buf, area)
    }

    /// Wait for the editor and read what it left in the file. Consumes the pane (the file is removed).
    pub fn finish(mut self) -> Outcome {
        let status = self.child.take().map_or_else(|| Err(io::Error::other("no process")), |mut c| c.wait());
        let text = std::fs::read_to_string(&self.path).map(|t| t.strip_suffix('\n').map(str::to_string).unwrap_or(t));
        let _ = std::fs::remove_file(&self.path);
        let (ok, status) = match status {
            Ok(s) => (s.success(), s.to_string()),
            Err(e) => (false, e.to_string()),
        };
        Outcome { text, ok, status }
    }
}

impl Drop for Pane {
    /// Closing claude++ (or cancelling) hangs the editor up like a closed terminal would.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if matches!(child.try_wait(), Ok(None)) {
                // SAFETY: signal our own child's session; it is the session leader (setsid at start).
                unsafe { libc::kill(-(child.id() as i32), libc::SIGHUP) };
                let _ = child.kill();
            }
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn draw_screen(screen: &vt100::Screen, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) else { continue };
            let text = cell.contents();
            dst.set_symbol(if text.is_empty() { " " } else { text });
            dst.set_style(style(cell));
        }
    }
    let (row, col) = screen.cursor_position();
    (!screen.hide_cursor() && row < area.height && col < area.width).then_some((area.x + col, area.y + row))
}

fn style(c: &vt100::Cell) -> Style {
    let color = |c: vt100::Color| match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    };
    let mut m = Modifier::empty();
    for (on, flag) in [(c.bold(), Modifier::BOLD), (c.dim(), Modifier::DIM), (c.italic(), Modifier::ITALIC), (c.underline(), Modifier::UNDERLINED), (c.inverse(), Modifier::REVERSED)] {
        if on {
            m |= flag;
        }
    }
    Style::default().fg(color(c.fgcolor())).bg(color(c.bgcolor())).add_modifier(m)
}

/// What a terminal sends for this key (xterm style). `app_cursor` is the editor's "application cursor keys" mode.
pub fn encode(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let (shift, alt, ctrl) = (k.modifiers.contains(KeyModifiers::SHIFT), k.modifiers.contains(KeyModifiers::ALT), k.modifiers.contains(KeyModifiers::CONTROL));
    // xterm's modifier parameter: 1 + shift 1 + alt 2 + ctrl 4.
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |final_byte: char, num: u8| -> Vec<u8> {
        if m > 1 { format!("\x1b[{num};{m}{final_byte}") } else if num == 1 { format!("\x1b[{final_byte}") } else { format!("\x1b[{num}{final_byte}") }.into_bytes()
    };
    let arrow = |c: char| -> Vec<u8> { if m == 1 && app_cursor { format!("\x1bO{c}").into_bytes() } else { csi(c, 1) } };
    let tilde = |n: u8| -> Vec<u8> { if m > 1 { format!("\x1b[{n};{m}~") } else { format!("\x1b[{n}~") }.into_bytes() };
    let with_alt = |mut b: Vec<u8>| -> Vec<u8> {
        if alt {
            b.insert(0, 0x1b);
        }
        b
    };
    match k.code {
        KeyCode::Char(c) if ctrl => {
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                ' ' | '@' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '7' | '/' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return vec![],
            };
            with_alt(vec![b])
        }
        KeyCode::Char(c) => with_alt(c.to_string().into_bytes()),
        KeyCode::Enter => with_alt(vec![b'\r']),
        KeyCode::Tab => with_alt(vec![b'\t']),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => with_alt(vec![if ctrl { 0x08 } else { 0x7f }]),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => arrow('A'),
        KeyCode::Down => arrow('B'),
        KeyCode::Right => arrow('C'),
        KeyCode::Left => arrow('D'),
        KeyCode::Home => arrow('H'),
        KeyCode::End => arrow('F'),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::F(n @ 1..=4) => {
            let c = [b'P', b'Q', b'R', b'S'][n as usize - 1] as char;
            if m > 1 { format!("\x1b[1;{m}{c}") } else { format!("\x1bO{c}") }.into_bytes()
        }
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn enc(code: KeyCode, mods: KeyModifiers) -> String {
        String::from_utf8_lossy(&encode(key(code, mods), false)).replace('\x1b', "ESC")
    }

    #[test]
    fn keys_become_what_a_terminal_sends() {
        let (n, c, a, s) = (KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT, KeyModifiers::SHIFT);
        assert_eq!(enc(KeyCode::Char('h'), n), "h");
        assert_eq!(enc(KeyCode::Char('é'), n), "é");
        assert_eq!(enc(KeyCode::Char('w'), c), "\u{17}");
        assert_eq!(enc(KeyCode::Char('x'), a), "ESCx");
        assert_eq!(enc(KeyCode::Enter, n), "\r");
        assert_eq!(enc(KeyCode::Backspace, n), "\u{7f}");
        assert_eq!(enc(KeyCode::Esc, n), "ESC");
        assert_eq!(enc(KeyCode::Up, n), "ESC[A");
        assert_eq!(enc(KeyCode::Left, c), "ESC[1;5D");
        assert_eq!(enc(KeyCode::Right, s | a), "ESC[1;4C");
        assert_eq!(enc(KeyCode::Delete, n), "ESC[3~");
        assert_eq!(enc(KeyCode::PageDown, n), "ESC[6~");
        assert_eq!(enc(KeyCode::F(1), n), "ESCOP");
        assert_eq!(enc(KeyCode::F(5), n), "ESC[15~");
        assert_eq!(enc(KeyCode::BackTab, s), "ESC[Z");
        assert_eq!(String::from_utf8_lossy(&encode(key(KeyCode::Up, n), true)), "\x1bOA", "application cursor mode");
    }

    #[test]
    fn no_key_combination_panics_the_encoder() {
        let codes = [KeyCode::Char('a'), KeyCode::Char('Z'), KeyCode::Char('\u{0}'), KeyCode::Char('日'), KeyCode::Char('\u{10ffff}'), KeyCode::Enter, KeyCode::Tab, KeyCode::BackTab, KeyCode::Backspace, KeyCode::Esc, KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right, KeyCode::Home, KeyCode::End, KeyCode::Insert, KeyCode::Delete, KeyCode::PageUp, KeyCode::PageDown, KeyCode::Null, KeyCode::CapsLock, KeyCode::F(0), KeyCode::F(1), KeyCode::F(4), KeyCode::F(5), KeyCode::F(12), KeyCode::F(13), KeyCode::F(255)];
        for code in codes {
            for bits in 0..8u8 {
                let mods = KeyModifiers::from_bits_truncate(bits);
                for app_cursor in [false, true] {
                    let _ = encode(key(code, mods), app_cursor);
                }
            }
        }
    }

    fn parsed(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser<Replies> {
        let mut p = vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 0, Replies::default());
        p.process(bytes);
        p
    }

    #[test]
    fn the_screen_is_drawn_with_colours_wide_characters_and_the_cursor() {
        let p = parsed(3, 10, "plain \x1b[1;31mred\x1b[0m\r\n日本\x1b[3;1Hz".as_bytes());
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        let cursor = draw_screen(p.screen(), &mut buf, Rect::new(0, 0, 10, 3));
        assert_eq!(buf[(0, 0)].symbol(), "p");
        let red = &buf[(6, 0)];
        assert_eq!((red.symbol(), red.fg, red.modifier.contains(Modifier::BOLD)), ("r", Color::Indexed(1), true));
        assert_eq!(buf[(0, 1)].symbol(), "日");
        assert_eq!(buf[(2, 1)].symbol(), "本");
        assert_eq!(cursor, Some((1, 2)), "after the z at row 3, column 1 (x, y)");
    }

    #[test]
    fn terminal_queries_get_answers() {
        let mut p = parsed(24, 80, b"\x1b[5;7H\x1b[6n\x1b[c\x1b[>c");
        let r = String::from_utf8(std::mem::take(&mut p.callbacks_mut().0)).unwrap();
        assert_eq!(r, "\x1b[5;7R\x1b[?62;c\x1b[>0;0;0c");
    }

    #[test]
    fn whatever_an_editor_prints_can_be_drawn_at_any_size() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let alphabet: &[&[u8]] = &[b"\x1b[", b"\x1b]", b"\x07", b"\x1b[?1049h", b"\x1b[?1049l", b"\x1b[999999;999999H", b"\x1b[38;2;1;2;3m", b"\x1b[48;5;300m", b"\x1b[2J", b"\x1b[999999L", b"\x1b[9999P", b"\x1b[1;1r", b"\xe6\x97\xa5", b"\xff\xfe", b"\xcc\x81", b"\r\n", b"\x08", b"a", b"\x1b[?25l", b"\x1b[8;1;1t", b"\x1b7", b"\x1b8", b"\x1bM", b"\x1b[3 q"];
        for round in 0..200 {
            let (rows, cols) = ((next() % 30 + 1) as u16, (next() % 90 + 1) as u16);
            let mut p = parsed(rows, cols, b"");
            for _ in 0..300 {
                p.process(alphabet[(next() % alphabet.len() as u64) as usize]);
                if next() % 40 == 0 {
                    p.set_size(nz((next() % 30 + 1) as u16), nz((next() % 90 + 1) as u16));
                }
            }
            for (w, h) in [(80u16, 24u16), (1, 1), (3, 200), (200, 3), (rows, cols)] {
                let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
                draw_screen(p.screen(), &mut buf, Rect::new(0, 0, w, h));
            }
            let _ = round;
        }
    }

    /// The whole path through a real pty and `sh`; skipped where a pty cannot be opened (a sandbox).
    #[test]
    fn a_program_on_a_pty_is_drawn_and_its_file_comes_back() {
        let (tx, rx) = std::sync::mpsc::channel();
        let editor = "printf 'hello from the editor' > \"$1\"; printf 'on screen' #";
        let pane = Pane::spawn(1, Use::Send, editor, "old", (4, 30), tx, |id, m| (id, m));
        let mut pane = match pane {
            Ok(p) => p,
            Err(e) => return eprintln!("skipped: no pty here ({e})"),
        };
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(10)).expect("the editor ended") {
                (_, Msg::Bytes(b)) => pane.feed(&b),
                (_, Msg::Ended) => break,
            }
        }
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 4));
        pane.render(&mut buf, Rect::new(0, 0, 30, 4));
        let row: String = (0..30).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(row.starts_with("on screen"), "{row:?}");
        let out = pane.finish();
        assert!(out.ok, "{}", out.status);
        assert_eq!(out.text.unwrap(), "hello from the editor");
    }
}
