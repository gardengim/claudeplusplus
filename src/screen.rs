//! The terminal's output, one frame at a time. ratatui writes a frame as many small pieces while the cursor
//! sits wherever the last frame left it, and only afterwards hides or moves it: through a terminal that
//! paints as bytes arrive (or through the line-buffered `Stdout`, which hands the terminal a frame in
//! chunks) the cursor is seen jumping about. So a frame is collected whole and sent as one write, inside
//! a synchronized update (`CSI ? 2026`, which terminals that lack it ignore) with the cursor hidden first.
//! A frame that only repeats the previous frame's cursor commands (nothing on screen changed) is not sent.

use std::io::{self, Write};

const BEGIN: &[u8] = b"\x1b[?2026h\x1b[?25l";
const END: &[u8] = b"\x1b[?2026l";
/// A frame this short holds no cell updates, only the reset and cursor commands ratatui ends every frame with.
const CURSOR_ONLY: usize = 64;

pub struct Frames<W: Write> {
    out: W,
    frame: Vec<u8>,
    /// The last frame sent if it was cursor-only; cleared by any frame that drew something.
    last_cursor_only: Vec<u8>,
}

impl<W: Write> Frames<W> {
    pub fn new(out: W) -> Self {
        Frames { out, frame: Vec::with_capacity(16 * 1024), last_cursor_only: vec![] }
    }
}

impl<W: Write> Write for Frames<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.frame.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.frame.is_empty() {
            return Ok(());
        }
        let small = self.frame.len() <= CURSOR_ONLY;
        if small && self.frame == self.last_cursor_only {
            self.frame.clear();
            return Ok(());
        }
        let mut wire = Vec::with_capacity(BEGIN.len() + self.frame.len() + END.len());
        wire.extend_from_slice(BEGIN);
        wire.extend_from_slice(&self.frame);
        wire.extend_from_slice(END);
        self.last_cursor_only = if small { std::mem::take(&mut self.frame) } else { vec![] };
        self.frame.clear();
        self.out.write_all(&wire)?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_goes_out_whole_inside_a_synchronized_update_and_an_unchanged_one_stays_home() {
        let mut f = Frames::new(Vec::new());
        f.write_all(b"\x1b[1;1Hhello").unwrap();
        f.write_all(b" world").unwrap();
        assert!(f.out.is_empty(), "nothing is sent before the frame ends");
        f.flush().unwrap();
        assert_eq!(f.out, b"\x1b[?2026h\x1b[?25l\x1b[1;1Hhello world\x1b[?2026l");
        let sent = f.out.len();
        let cursor = b"\x1b[0m\x1b[?25h\x1b[3;4H";
        for want in [sent + BEGIN.len() + cursor.len() + END.len(), sent + BEGIN.len() + cursor.len() + END.len(), sent + BEGIN.len() + cursor.len() + END.len()] {
            f.write_all(cursor).unwrap();
            f.flush().unwrap();
            assert_eq!(f.out.len(), want, "the first cursor-only frame is sent, the repeats are not");
        }
        f.write_all(b"\x1b[1;1Hbye").unwrap();
        f.flush().unwrap();
        f.write_all(cursor).unwrap();
        let before = f.out.len();
        f.flush().unwrap();
        assert!(f.out.len() > before, "after a frame that drew, the cursor is sent again");
    }
}
