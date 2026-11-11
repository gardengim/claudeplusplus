//! The terminal's output, one frame at a time. ratatui writes a frame as many small pieces while the cursor
//! sits wherever the last frame left it, and only afterwards hides or moves it: through a terminal that
//! paints as bytes arrive the cursor is seen jumping about. Worse, its crossterm backend flushes after
//! *every* cursor command, so one `draw` reaches the terminal as several chunks: cells, then "show cursor"
//! while the cursor still sits at the end of the last cell written, then the move to the input box. So the
//! writer ignores `flush`: a frame is collected whole and [`Frames::present`] (called after each `draw`) sends
//! it as one write, inside a synchronized update (`CSI ? 2026`, which terminals that lack it ignore). A
//! frame that draws cells hides a visible cursor first and restores it after; one that only moves the
//! cursor does not touch its visibility, and one that only repeats the previous frame's cursor commands
//! (nothing on screen changed) is not sent at all.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

const BEGIN: &[u8] = b"\x1b[?2026h";
const SHOW: &[u8] = b"\x1b[?25h";
const HIDE: &[u8] = b"\x1b[?25l";
const END: &[u8] = b"\x1b[?2026l";

/// Whether the frame prints anything: it is more than the resets and cursor commands ratatui ends every frame with.
fn draws(frame: &[u8]) -> bool {
    let mut i = 0;
    while i < frame.len() {
        if frame[i] == 0x1b {
            i += 1;
            if frame.get(i) == Some(&b'[') {
                i += 1;
                while i < frame.len() && !(0x40..=0x7e).contains(&frame[i]) {
                    i += 1;
                }
            }
        } else if frame[i] >= 0x20 {
            return true;
        }
        i += 1;
    }
    false
}

pub struct Inner<W: Write> {
    out: W,
    frame: Vec<u8>,
    /// The last frame sent if it was cursor-only; cleared by any frame that drew something.
    last_cursor_only: Vec<u8>,
    /// Whether ratatui last asked for the cursor to be visible. It tracks that itself and sends a show only on a change, so a frame
    /// that merely moves the cursor says nothing about visibility, and our own hide at the start of the frame would stick.
    visible: bool,
}

impl<W: Write> Inner<W> {
    pub fn new(out: W) -> Self {
        Inner { out, frame: Vec::with_capacity(16 * 1024), last_cursor_only: vec![], visible: false }
    }
}

impl<W: Write> Write for Inner<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.frame.extend_from_slice(b);
        Ok(b.len())
    }

    /// Does nothing: ratatui flushes mid-frame (see the module docs). [`Frames::present`] sends.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Inner<W> {
    /// Send what has been collected since the last call as one synchronized update.
    pub fn present(&mut self) -> io::Result<()> {
        if self.frame.is_empty() {
            return Ok(());
        }
        let small = !draws(&self.frame);
        if small && self.frame == self.last_cursor_only {
            self.frame.clear();
            return Ok(());
        }
        let asked = self.frame.windows(SHOW.len()).rposition(|w| w == SHOW || w == HIDE).map(|i| &self.frame[i..i + SHOW.len()] == SHOW);
        // Cells are drawn with the cursor travelling over them: keep it out of sight meanwhile.
        let hide = !small && self.visible;
        let restore = hide && asked.is_none();
        self.visible = asked.unwrap_or(self.visible);
        let mut wire = Vec::with_capacity(BEGIN.len() + self.frame.len() + HIDE.len() + SHOW.len() + END.len());
        wire.extend_from_slice(BEGIN);
        if hide {
            wire.extend_from_slice(HIDE);
        }
        wire.extend_from_slice(&self.frame);
        if restore {
            wire.extend_from_slice(SHOW);
        }
        wire.extend_from_slice(END);
        self.last_cursor_only = if small { std::mem::take(&mut self.frame) } else { vec![] };
        self.frame.clear();
        self.out.write_all(&wire)?;
        self.out.flush()
    }
}

impl<W: Write> Drop for Inner<W> {
    fn drop(&mut self) {
        let _ = self.present(); // e.g. the cursor ratatui shows again when the terminal is dropped
    }
}

/// The writer handed to ratatui. Clone it first and keep the clone: calling [`Frames::present`] on that
/// sends the frame ratatui just drew through the one it owns.
pub struct Frames<W: Write>(Rc<RefCell<Inner<W>>>);

impl<W: Write> Frames<W> {
    pub fn new(out: W) -> Self {
        Frames(Rc::new(RefCell::new(Inner::new(out))))
    }
    pub fn present(&self) -> io::Result<()> {
        self.0.borrow_mut().present()
    }
}

impl<W: Write> Clone for Frames<W> {
    fn clone(&self) -> Self {
        Frames(self.0.clone())
    }
}

impl<W: Write> Write for Frames<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // see the module docs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(f: &mut Inner<Vec<u8>>, b: &[u8]) -> String {
        let before = f.out.len();
        f.write_all(b).unwrap();
        f.flush().unwrap(); // ratatui flushes mid-frame; nothing may go out
        assert_eq!(f.out.len(), before, "flush alone sends nothing");
        f.present().unwrap();
        String::from_utf8_lossy(&f.out[before..]).into_owned()
    }

    #[test]
    fn a_frame_goes_out_whole_inside_a_synchronized_update_and_an_unchanged_one_stays_home() {
        let mut f = Inner::new(Vec::new());
        f.write_all(b"\x1b[1;1Hhello").unwrap();
        f.write_all(b" world").unwrap();
        assert!(f.out.is_empty(), "nothing is sent before the frame ends");
        f.present().unwrap();
        assert_eq!(f.out, b"\x1b[?2026h\x1b[1;1Hhello world\x1b[?2026l");
        let cursor = b"\x1b[0m\x1b[?25h\x1b[3;4H";
        assert!(!send(&mut f, cursor).is_empty(), "the first cursor-only frame is sent");
        assert_eq!(send(&mut f, cursor), "", "and its repeats are not");
        assert_eq!(send(&mut f, cursor), "");
        let drew = send(&mut f, b"\x1b[1;1Hbye\x1b[0m\x1b[?25h\x1b[3;4H");
        assert_eq!(drew, "\x1b[?2026h\x1b[?25l\x1b[1;1Hbye\x1b[0m\x1b[?25h\x1b[3;4H\x1b[?2026l", "a visible cursor is hidden while cells are drawn");
        assert!(!send(&mut f, cursor).is_empty(), "after a frame that drew, the cursor is sent again");
    }

    #[test]
    fn a_cursor_ratatui_did_not_mention_is_left_as_it_was() {
        let mut g = Inner::new(Vec::new());
        send(&mut g, b"\x1b[0m\x1b[?25h\x1b[3;4H");
        assert_eq!(send(&mut g, b"\x1b[5;6H"), "\x1b[?2026h\x1b[5;6H\x1b[?2026l", "a move alone neither hides nor shows");
        let drew = send(&mut g, b"\x1b[1;1Hx");
        assert_eq!(drew, "\x1b[?2026h\x1b[?25l\x1b[1;1Hx\x1b[?25h\x1b[?2026l", "cells drawn, cursor put back");
        send(&mut g, b"\x1b[?25l");
        assert_eq!(send(&mut g, b"\x1b[1;1Hy"), "\x1b[?2026h\x1b[1;1Hy\x1b[?2026l", "a cursor ratatui hid stays hidden, with no needless hide");
    }
}
