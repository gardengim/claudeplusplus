//! Inline images through the kitty graphics protocol, using *unicode placeholders*: the image is sent
//! to the terminal once, and the transcript then holds ordinary text cells (U+10EEEE plus row/column
//! diacritics, coloured with the image id). The terminal draws the picture into those cells, so
//! scrolling, clipping and redraws need no special handling and the app keeps only an id, not pixels.
//! Supported by kitty and Ghostty; anywhere else images degrade to a one-line label.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Combining marks the protocol uses to number rows and columns (kitty's rowcolumn-diacritics.txt).
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187,
    0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

const MAX_COLS: u32 = 48;
const MAX_ROWS: u32 = 14;
/// Larger or non-PNG images are converted/downscaled first; this bounds what is sent to the terminal.
const MAX_PX: u32 = 1024;
const PASS_THROUGH_BYTES: usize = 1_500_000;

/// A picture already sent to the terminal, to be drawn as a `cols` x `rows` block of text cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shown {
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
    pub w: u32,
    pub h: u32,
}

/// Whether to draw images. `CLAUDEPP_IMAGES=on|off` overrides detection; tmux/screen swallow the
/// protocol, so they are never auto-enabled.
pub fn enabled() -> bool {
    if cfg!(test) {
        return false; // unit tests must never write graphics escapes into a real terminal
    }
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    // The environment variable beats the saved setting, which beats detection.
    match env("CLAUDEPP_IMAGES").as_str() {
        "on" => return true,
        "off" => return false,
        _ => {}
    }
    match MODE.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let term = env("TERM");
    if !env("TMUX").is_empty() || term.starts_with("screen") || term.starts_with("tmux") {
        return false;
    }
    env("TERM_PROGRAM") == "ghostty" || term.contains("ghostty") || term.contains("kitty") || !env("KITTY_WINDOW_ID").is_empty()
}

/// The saved setting: 0 = auto (detect), 1 = on, 2 = off.
static MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_mode(name: &str) {
    MODE.store(match name { "on" => 1, "off" => 2, _ => 0 }, Ordering::Relaxed);
}

pub fn mode() -> &'static str {
    match MODE.load(Ordering::Relaxed) {
        1 => "on",
        2 => "off",
        _ => "auto",
    }
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Width and height from a PNG's IHDR chunk.
fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[..8] != b"\x89PNG\r\n\x1a\n" || &b[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    Some((be(16), be(20)))
}

/// PNG bytes the terminal can show. The protocol takes PNG only, so anything else (and anything
/// large) goes through macOS `sips`; without it, only small PNGs are shown.
fn to_png(bytes: &[u8], media: &str) -> Option<Vec<u8>> {
    if media == "image/png" && bytes.len() <= PASS_THROUGH_BYTES && png_size(bytes).is_some() {
        return Some(bytes.to_vec());
    }
    convert(bytes, MAX_PX)
}

/// Re-encode as a PNG no larger than `px` on its long edge, using macOS `sips`.
fn convert(bytes: &[u8], px: u32) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-img-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let (src, dst) = (dir.join("in"), dir.join("out.png"));
    std::fs::write(&src, bytes).ok()?;
    let ok = std::process::Command::new("sips")
        .args(["-Z", &px.to_string(), "-s", "format", "png"])
        .arg(&src)
        .arg("--out")
        .arg(&dst)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let out = ok.then(|| std::fs::read(&dst).ok()).flatten();
    let _ = std::fs::remove_dir_all(&dir);
    out.filter(|p| png_size(p).is_some())
}

/// A smaller PNG of the same picture, for images over the API's size limit.
pub fn shrink(bytes: &[u8]) -> Option<Vec<u8>> {
    convert(bytes, 1568)
}

pub fn media_type(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// The image on the system clipboard as PNG bytes, if there is one. macOS asks AppleScript for the
/// clipboard as a PNG; elsewhere `wl-paste` or `xclip` are tried.
pub fn clipboard_png() -> Option<Vec<u8>> {
    let bytes = clipboard_raw()?;
    png_size(&bytes).is_some().then_some(bytes)
}

#[cfg(target_os = "macos")]
fn clipboard_raw() -> Option<Vec<u8>> {
    let path = std::env::temp_dir().join(format!("claudeplusplus-clip-{}.png", std::process::id()));
    let script = format!(
        "set d to (the clipboard as \u{ab}class PNGf\u{bb})\nset f to open for access POSIX file \"{}\" with write permission\nset eof of f to 0\nwrite d to f\nclose access f",
        path.display()
    );
    let ok = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let out = ok.then(|| std::fs::read(&path).ok()).flatten();
    let _ = std::fs::remove_file(&path);
    out
}

#[cfg(not(target_os = "macos"))]
fn clipboard_raw() -> Option<Vec<u8>> {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
    };
    run("wl-paste", &["--type", "image/png"]).or_else(|| run("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]))
}

/// Cell size in pixels, from the terminal when it reports it, else a typical 1:2 cell.
fn cell_px() -> (f64, f64) {
    ratatui::crossterm::terminal::window_size()
        .ok()
        .filter(|w| w.columns > 0 && w.rows > 0 && w.width > 0 && w.height > 0)
        .map_or((8.0, 16.0), |w| (w.width as f64 / w.columns as f64, w.height as f64 / w.rows as f64))
}

/// Cells to give a `w` x `h` picture so it keeps its aspect ratio inside the size limits.
fn fit(w: u32, h: u32, avail_cols: u32, max_rows: u32, cell: (f64, f64)) -> (u16, u16) {
    let max_c = MAX_COLS.min(avail_cols).max(1) as f64;
    let ratio = h as f64 * cell.0 / (w as f64 * cell.1); // rows per column
    let (mut c, mut r) = (max_c, (max_c * ratio).ceil());
    if r > max_rows as f64 {
        r = max_rows as f64;
        c = (r / ratio).ceil().min(max_c);
    }
    (c.max(1.0) as u16, r.max(1.0) as u16)
}

static NEXT: AtomicU32 = AtomicU32::new(1);
static SENT: AtomicBool = AtomicBool::new(false);

fn next_id() -> u32 {
    // Distinct per process (two claude++ in one terminal) and nonzero; the id is carried in a 24-bit colour.
    ((std::process::id() & 0xFFF) << 12 | (NEXT.fetch_add(1, Ordering::Relaxed) & 0xFFF)).max(1)
}

/// The escape sequences that upload a PNG and create its virtual placement.
fn transmit_seq(id: u32, png_b64: &str, cols: u16, rows: u16) -> String {
    let mut out = String::new();
    let chunks: Vec<&str> = png_b64.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap()).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = (i + 1 < chunks.len()) as u8;
        if i == 0 {
            out += &format!("\x1b_Ga=t,f=100,t=d,i={id},q=2,m={more};{c}\x1b\\");
        } else {
            out += &format!("\x1b_Gm={more},q=2;{c}\x1b\\");
        }
    }
    out + &format!("\x1b_Ga=p,U=1,i={id},c={cols},r={rows},q=2\x1b\\")
}

/// Send an image to the terminal and return how to draw it. None = cannot be shown (not decodable,
/// no converter): the caller falls back to a text label.
pub fn show(bytes: &[u8], media: &str, avail_cols: u32) -> Option<Shown> {
    upload(&prepare(bytes, media)?, avail_cols, MAX_ROWS)
}

/// Bytes the terminal can show (PNG), or None.
pub fn prepare(bytes: &[u8], media: &str) -> Option<Vec<u8>> {
    to_png(bytes, media)
}

/// Upload already-prepared PNG bytes, to be drawn at most `avail_cols` x `max_rows` cells.
pub fn upload(png: &[u8], avail_cols: u32, max_rows: u32) -> Option<Shown> {
    let (w, h) = png_size(png)?;
    let (cols, rows) = fit(w, h, avail_cols, max_rows, cell_px());
    let id = next_id();
    let seq = transmit_seq(id, &base64(png), cols, rows);
    let mut out = std::io::stdout().lock();
    out.write_all(seq.as_bytes()).ok()?;
    out.flush().ok()?;
    SENT.store(true, Ordering::Relaxed);
    Some(Shown { id, cols, rows, w, h })
}

/// Free one uploaded image (its placeholders then draw nothing).
pub fn delete(id: u32) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
    let _ = out.flush();
}

/// Remove every image this terminal screen holds (on exit).
pub fn clear_all() {
    if !SENT.load(Ordering::Relaxed) {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\");
    let _ = out.flush();
}

/// The text cells that the terminal replaces with the picture.
pub fn placeholder_lines(s: &Shown) -> Vec<Line<'static>> {
    let color = Color::Rgb((s.id >> 16) as u8, (s.id >> 8) as u8, s.id as u8);
    (0..s.rows as usize)
        .map(|r| {
            let row = char::from_u32(DIACRITICS[r.min(DIACRITICS.len() - 1)]).unwrap();
            let text: String = (0..s.cols as usize)
                .flat_map(|c| ['\u{10EEEE}', row, char::from_u32(DIACRITICS[c.min(DIACRITICS.len() - 1)]).unwrap()])
                .collect();
            Line::from(Span::styled(text, Style::new().fg(color)))
        })
        .collect()
}

// ---------- inline `[Image #N]` tokens ----------

/// The text that stands for pasted image `n` inside a message.
pub fn token(n: u32) -> String {
    format!("[Image #{n}]")
}

/// `Some(n)` when `s` is exactly one token.
pub fn token_n(s: &str) -> Option<u32> {
    let n = s.strip_prefix("[Image #")?.strip_suffix(']')?;
    (!n.is_empty() && n.len() < 6 && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse().ok())?
}

pub enum Piece {
    Text(String),
    Image(u32),
}

/// Split a message at its tokens. A token only counts when `known(n)` (an image really is attached).
pub fn split_tokens(text: &str, known: impl Fn(u32) -> bool) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut buf = String::new();
    while let Some(i) = rest.find("[Image #") {
        let Some(len) = rest[i..].find(']') else { break };
        match token_n(&rest[i..=i + len]).filter(|n| known(*n)) {
            Some(n) => {
                buf.push_str(&rest[..i]);
                out.push(Piece::Text(std::mem::take(&mut buf)));
                out.push(Piece::Image(n));
            }
            None => buf.push_str(&rest[..=i + len]),
        }
        rest = &rest[i + len + 1..];
    }
    buf.push_str(rest);
    out.push(Piece::Text(buf));
    out
}

/// Numbers of all tokens in `text`.
pub fn tokens_in(text: &str) -> Vec<u32> {
    split_tokens(text, |_| true).into_iter().filter_map(|p| if let Piece::Image(n) = p { Some(n) } else { None }).collect()
}

/// The message without its tokens, for titles and history.
pub fn strip_tokens(text: &str) -> String {
    split_tokens(text, |_| true)
        .into_iter()
        .filter_map(|p| if let Piece::Text(t) = p { Some(t) } else { None })
        .collect::<String>()
        .split('\n')
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Length in chars of a token that ends exactly at `at` (so backspace removes it whole).
pub fn token_ending_at(chars: &[char], at: usize) -> Option<usize> {
    if at == 0 || chars.get(at - 1) != Some(&']') {
        return None;
    }
    let start = chars[..at].iter().rposition(|c| *c == '[')?;
    let s: String = chars[start..at].iter().collect();
    token_n(&s).map(|_| at - start)
}

/// Length in chars of a token that starts exactly at `at` (so delete removes it whole).
pub fn token_starting_at(chars: &[char], at: usize) -> Option<usize> {
    if chars.get(at) != Some(&'[') {
        return None;
    }
    let end = at + chars[at..].iter().position(|c| *c == ']')? + 1;
    let s: String = chars[at..end].iter().collect();
    token_n(&s).map(|_| end - at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 transparent PNG.
    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn base64_round_trips_and_matches_rfc4648() {
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(unbase64("Zm9vYg==").unwrap(), b"foob");
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(unbase64(&base64(&data)).unwrap(), data);
        assert!(unbase64("a$b").is_none());
    }

    #[test]
    fn png_header_gives_size_and_passes_through_unconverted() {
        let png = unbase64(PNG_1X1).unwrap();
        assert_eq!(png_size(&png), Some((1, 1)));
        assert_eq!(to_png(&png, "image/png").unwrap(), png);
        assert_eq!(png_size(b"not a png at all, just text...."), None);
    }

    #[test]
    fn fit_keeps_aspect_within_limits() {
        let cell = (9.0, 18.0);
        assert_eq!(fit(100, 100, 200, 14, cell), (28, 14)); // square picture, 2:1 cells: 14 rows -> 28 cols
        assert_eq!(fit(100, 100, 200, 5, cell), (10, 5), "a smaller row limit shrinks the picture, same shape");
        let (c, r) = fit(1000, 1000, 200, 14, cell); // square → limited by rows
        assert!(r <= 14 && c <= 48 && (c as i32 - 2 * r as i32).abs() <= 2, "{c}x{r}");
        let (c, r) = fit(3000, 100, 200, 14, cell); // wide → full columns, few rows
        assert_eq!(c, 48);
        assert!(r <= 3);
        assert_eq!(fit(100, 100, 10, 14, cell).0, 10); // narrow terminal
    }

    #[test]
    fn upload_is_chunked_quietly_and_placed_virtually() {
        let seq = transmit_seq(7, &"A".repeat(9000), 20, 5);
        assert!(seq.starts_with("\x1b_Ga=t,f=100,t=d,i=7,q=2,m=1;"));
        assert_eq!(seq.matches("\x1b_Gm=1,q=2;").count(), 1);
        assert_eq!(seq.matches("\x1b_Gm=0,q=2;").count(), 1);
        assert!(seq.ends_with("\x1b_Ga=p,U=1,i=7,c=20,r=5,q=2\x1b\\"));
        assert!(!seq.contains("q=0"), "responses would land in our input stream");
    }

    #[test]
    fn placeholders_carry_id_in_colour_and_number_cells() {
        let s = Shown { id: 0x0001_2345, cols: 3, rows: 2, w: 1, h: 1 };
        let lines = placeholder_lines(&s);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Rgb(0x01, 0x23, 0x45)));
        let cells: Vec<char> = lines[1].spans[0].content.chars().collect();
        assert_eq!(cells.len(), 9);
        assert_eq!((cells[0], cells[1], cells[2]), ('\u{10EEEE}', char::from_u32(DIACRITICS[1]).unwrap(), char::from_u32(DIACRITICS[0]).unwrap()));
        assert_eq!(cells[8], char::from_u32(DIACRITICS[2]).unwrap());
        // ratatui must see each placeholder as one one-column cell.
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 3, 1));
        buf.set_line(0, 0, &lines[0], 3);
        assert_eq!(buf[(2, 0)].symbol().chars().next(), Some('\u{10EEEE}'));
        assert_eq!(buf[(2, 0)].symbol().chars().count(), 3);
    }

    #[test]
    fn tokens_split_known_images_only_and_survive_odd_text() {
        let known = |n: u32| n == 1 || n == 2;
        let show = |t: &str| {
            split_tokens(t, known)
                .into_iter()
                .map(|p| match p {
                    Piece::Text(t) => format!("T({t})"),
                    Piece::Image(n) => format!("I{n}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        };
        assert_eq!(show("a\n[Image #1]\nb"), "T(a\n)|I1|T(\nb)");
        assert_eq!(show("[Image #1][Image #2]"), "T()|I1|T()|I2|T()");
        assert_eq!(show("typed [Image #9] by hand"), "T(typed [Image #9] by hand)", "unknown number stays text");
        assert_eq!(show("broken [Image #1 and [Image #"), "T(broken [Image #1 and [Image #)");
        assert_eq!(show("[Image #x] [Image #]"), "T([Image #x] [Image #])");
        assert_eq!(show("no tokens"), "T(no tokens)");
        assert_eq!(tokens_in("x [Image #3] y [Image #12]"), vec![3, 12]);
        assert_eq!(strip_tokens("see\n[Image #1]\nthis  \n"), "see\nthis");
        assert_eq!(token_n("[Image #7]"), Some(7));
        assert_eq!(token_n("[Image #7] "), None);
    }

    #[test]
    fn backspace_and_delete_find_whole_tokens() {
        let c: Vec<char> = "ab[Image #12]cd".chars().collect();
        assert_eq!(token_ending_at(&c, 13), Some(11));
        assert_eq!(token_ending_at(&c, 12), None, "cursor inside the token");
        assert_eq!(token_ending_at(&c, 2), None);
        assert_eq!(token_starting_at(&c, 2), Some(11));
        assert_eq!(token_starting_at(&c, 3), None);
        let odd: Vec<char> = "[not a token]".chars().collect();
        assert_eq!(token_ending_at(&odd, 13), None);
    }
}
