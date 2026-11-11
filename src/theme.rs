//! The look of claude++, by meaning. Surfaces ask for a role ("this row is selected", "the agent is waiting
//! on you") and never name a colour, so the whole UI can be re-skinned, or checked for consistency, in this
//! one file. See DESIGN.md for the rules these encode.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// What the terminal last said its background is, and whether it ever answered.
static LIGHT: AtomicBool = AtomicBool::new(false);
static ANSWERED: AtomicBool = AtomicBool::new(false);
/// `/config` → Colors: 0 auto (follow the terminal), 1 dark, 2 light.
static MODE: AtomicU8 = AtomicU8::new(0);
const MODES: [&str; 3] = ["auto", "dark", "light"];

/// Background bands (your messages, diff lines) need to know the terminal's background; text colours do not.
pub fn set_light(light: bool) {
    LIGHT.store(light, Ordering::Relaxed);
    ANSWERED.store(true, Ordering::Relaxed);
}

/// What the terminal last reported, None if it never answered.
pub fn detected() -> Option<bool> {
    ANSWERED.load(Ordering::Relaxed).then(|| LIGHT.load(Ordering::Relaxed))
}

pub fn set_mode(name: &str) {
    MODE.store(MODES.iter().position(|m| *m == name).unwrap_or(0) as u8, Ordering::Relaxed);
}

pub fn mode_name() -> &'static str {
    MODES[MODE.load(Ordering::Relaxed) as usize]
}

/// Whether the colours follow the terminal (and so are worth re-checking when it switches light/dark).
pub fn follows_terminal() -> bool {
    MODE.load(Ordering::Relaxed) == 0
}

fn light() -> bool {
    match MODE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => LIGHT.load(Ordering::Relaxed),
    }
}

/// Text picked with the mouse: a quiet blue behind it, the text keeps its own colours.
pub fn selection() -> Style {
    Style::new().bg(if light() { Color::Indexed(153) } else { Color::Indexed(24) })
}

/// Text on a band. A band sets its own background, so it must set its text colour too: left to the
/// terminal's default, text turns invisible when the terminal switches light/dark under a band chosen
/// for the other one (black on dark grey). With both set, a band reads in any mode, even a stale one.
fn on_band(bg: Color) -> Style {
    Style::new().bg(bg).fg(if light() { Color::Indexed(235) } else { Color::Indexed(252) })
}

/// Ask the terminal for its background colour (OSC 11). A DA1 query goes right behind it: every terminal
/// answers that one, so the read always ends (no stray reader left to swallow keys) and a terminal that
/// ignores OSC 11 costs one round trip. The deadline only matters when neither answers: a slow link gets a
/// full second, so a late reply is not left behind to arrive as typed keys. Raw mode must be on and nothing
/// else reading the terminal yet. None: no answer, or not a colour.
pub fn query_light_background() -> Option<bool> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    tty.write_all(b"\x1b]11;?\x07\x1b[c").ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
        let mut fd = libc::pollfd { fd: tty.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        if left <= 0 || unsafe { libc::poll(&mut fd, 1, left) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        match tty.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        let da1 = buf.windows(3).position(|w| w == b"\x1b[?");
        if da1.is_some_and(|i| buf[i..].contains(&b'c')) {
            break;
        }
    }
    background_is_light(&String::from_utf8_lossy(&buf))
}

/// `rgb:RRRR/GGGG/BBBB` (1 to 4 hex digits a channel) in an OSC 11 reply → whether it is a light colour.
fn background_is_light(reply: &str) -> Option<bool> {
    let rgb = &reply[reply.find("rgb:")? + 4..];
    let mut ch = rgb.split('/').take(3).map(|c| {
        let hex: String = c.chars().take_while(char::is_ascii_hexdigit).collect();
        let max = 16f64.powi(hex.len() as i32) - 1.0;
        u32::from_str_radix(&hex, 16).ok().map(|v| v as f64 / max)
    });
    let (r, g, b) = (ch.next()??, ch.next()??, ch.next()??);
    Some(0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5)
}

/// The band behind your messages in the transcript.
pub fn user_band() -> Style {
    on_band(if light() { Color::Indexed(254) } else { Color::Indexed(236) })
}

/// Whether the terminal says it takes 24-bit colour; the 256-colour palette has no quiet tints.
fn truecolor() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit"))
}

/// Added and removed lines of a diff: a tinted band, so a change reads at a glance even in a long hunk.
pub fn added_line() -> Style {
    on_band(match (light(), truecolor()) {
        (false, true) => Color::Rgb(16, 58, 30),
        (true, true) => Color::Rgb(218, 251, 225),
        (false, false) => Color::Indexed(22),
        (true, false) => Color::Indexed(194),
    })
}

/// The band behind a tool call that runs a command or touches a file (`$` Bash, `≡` Read/Glob/Grep, `±`/`+`
/// Edit/Write), header to last output line: tinted by the kind's hue, so one call reads as one block and a
/// run of calls separates. Other tools get none. Diff lines keep their own green and red on top of it.
pub fn tool_band(name: &str) -> Option<Style> {
    let kind = match name {
        "Bash" => 0,
        "Read" | "Glob" | "Grep" => 1,
        "Edit" | "Write" | "NotebookEdit" => 2,
        _ => return None,
    };
    let (dark, light_) = match kind {
        0 => ((22, 32, 48), (228, 238, 252)),
        1 => ((38, 26, 46), (244, 232, 250)),
        _ => ((46, 34, 20), (252, 240, 224)),
    };
    let rgb = |(r, g, b)| Color::Rgb(r, g, b);
    Some(on_band(match (light(), truecolor()) {
        (false, true) => rgb(dark),
        (true, true) => rgb(light_),
        (false, false) => Color::Indexed(235),
        (true, false) => Color::Indexed(255),
    }))
}

pub fn removed_line() -> Style {
    on_band(match (light(), truecolor()) {
        (false, true) => Color::Rgb(72, 22, 28),
        (true, true) => Color::Rgb(255, 228, 228),
        (false, false) => Color::Indexed(52),
        (true, false) => Color::Indexed(224),
    })
}

/// Code in a fenced block with a language: keywords, strings, numbers and comments. Darker shades on a
/// light background so they keep their contrast.
pub fn syn_keyword() -> Style {
    Style::new().fg(if light() { Color::Indexed(26) } else { Color::Indexed(75) })
}

pub fn syn_string() -> Style {
    Style::new().fg(if light() { Color::Indexed(28) } else { Color::Indexed(114) })
}

pub fn syn_number() -> Style {
    Style::new().fg(if light() { Color::Indexed(130) } else { Color::Indexed(180) })
}

pub fn syn_comment() -> Style {
    dim().add_modifier(Modifier::ITALIC)
}

/// The agent at work: the spinner above the input (Claude's own orange, as in Claude Code).
pub fn working() -> Style {
    Style::new().fg(Color::Indexed(173))
}

/// The glint that sweeps the word beside the spinner: lighter than `working` on a dark background, darker on a light one.
pub fn working_glint() -> Style {
    Style::new().fg(Color::Indexed(if light() { 130 } else { 223 })).add_modifier(Modifier::BOLD)
}

/// The colours `/color` offers, as 256-colour indexes (every terminal has them).
/// Each with a darker shade for a light background, where the bright one washes out (yellow on white).
const PALETTE: [(&str, u8, u8); 8] = [
    ("red", 203, 160),
    ("blue", 69, 26),
    ("green", 77, 28),
    ("yellow", 178, 136),
    ("purple", 135, 91),
    ("orange", 208, 166),
    ("pink", 211, 162),
    ("cyan", 44, 31),
];

/// The names `/color` offers, in order.
pub fn color_names() -> impl Iterator<Item = &'static str> {
    PALETTE.iter().map(|p| p.0)
}

/// Whether `name` is one of the colours `/color` accepts.
pub fn is_color(name: &str) -> bool {
    PALETTE.iter().any(|p| p.0 == name)
}

/// The conversation in front's colour, as its place in PALETTE plus one (0: none). Its shade is picked when
/// drawn, so a light/dark switch reaches it too.
static ACCENT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_accent(name: Option<&str>) {
    ACCENT.store(index(name), std::sync::atomic::Ordering::Relaxed);
}

/// The palette index of a colour name; 0 for none.
fn index(name: Option<&str>) -> u8 {
    name.and_then(|n| PALETTE.iter().position(|p| p.0 == n)).map_or(0, |i| i as u8 + 1)
}

/// The shade of PALETTE entry `index` (as from `index`, so never 0) for the current background.
fn shade(index: u8) -> Color {
    let p = PALETTE[index as usize - 1];
    Color::Indexed(if light() { p.2 } else { p.1 })
}

/// Colour number `i` of a series (`/context`: one per kind of thing), the `/color` palette in its shade for
/// the current background, wrapping.
pub fn series(i: usize) -> Color {
    shade((i % PALETTE.len()) as u8 + 1)
}

/// `accent` as one conversation's colour, for drawing a tab that is not in front.
pub fn tint(name: Option<&str>) -> Style {
    match index(name) {
        0 => Style::new().fg(Color::Cyan),
        n => Style::new().fg(shade(n)),
    }
}

/// You, and anything you control: your messages, the cursor, the focused tab. Cyan, or the colour the
/// conversation in front was given with `/color`.
pub fn accent() -> Style {
    match ACCENT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => tint(None),
        n => Style::new().fg(shade(n)),
    }
}

/// The agent is blocked waiting for you (permission, question): the one place yellow is used.
pub fn attention() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn ok() -> Style {
    Style::new().fg(Color::Green)
}

pub fn danger() -> Style {
    Style::new().fg(Color::Red)
}

/// Secondary text: hints, descriptions, separators, anything that should recede.
pub fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// The row the cursor is on: reversed, in the conversation's `/color` when it has one (a bar of that colour).
pub fn selected() -> Style {
    match ACCENT.load(std::sync::atomic::Ordering::Relaxed) {
        0 => Style::new().add_modifier(Modifier::REVERSED),
        _ => accent().add_modifier(Modifier::REVERSED),
    }
}

/// Code, commands and file contents.
pub fn code() -> Style {
    Style::new().fg(Color::LightBlue)
}

/// Per-kind hues for tool icons, so a run of tool calls separates at a glance. The line's own colour
/// still says how the call went; the icon says what kind it was.
pub fn tool_hue(name: &str) -> Style {
    Style::new().fg(match name {
        "Bash" => Color::LightBlue,
        "Read" | "Glob" | "Grep" => Color::Magenta,
        "Edit" | "Write" | "NotebookEdit" => Color::Indexed(208),
        "WebFetch" | "WebSearch" => Color::Indexed(37),
        _ => Color::Indexed(if light() { 97 } else { 141 }),
    })
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// How the permission mode is shown in the status line.
pub fn mode(mode: &str) -> Style {
    match mode {
        "acceptEdits" => Style::new().fg(Color::Magenta),
        "plan" => accent(),
        "auto" => ok(),
        "bypassPermissions" => danger(),
        _ => dim(),
    }
}

/// Marker for "the value that is in effect now", in every list (`off●  on`, the active model...).
pub const CURRENT: &str = "●";
/// Marker in front of the row the cursor is on in choice lists.
pub const CURSOR: &str = "❯";
/// Where the cursor of a menu that does not have the keys rests.
pub const CURSOR_AWAY: &str = "›";
/// Checkbox states, the same in every list: shown, hidden, some of the children, shortened.
pub const ON: &str = "☑";
pub const OFF: &str = "☐";
pub const PART: &str = "▣";
pub const BRIEF: &str = "◑";
/// Rule verdicts. Single-width glyphs only: no emoji, so columns never shift.
pub const ALLOW: &str = "✓";
pub const ASK: &str = "?";
pub const DENY: &str = "✗";
/// Tree rows, closed and open.
pub const CLOSED: &str = "▸";
pub const OPEN: &str = "▾";

/// One choice in a row of choices: a dot marks the value in effect (in `hue`), the others recede, and the
/// choice the cursor is on is reversed.
pub fn chip(label: &str, current: bool, cursor: bool, hue: Style) -> Span<'static> {
    chip_in(label, current, cursor, true, hue)
}

/// [`chip`] in a menu that may not have the keys: the choice under its cursor is only underlined then, and
/// reversed once Tab has brought the menu into focus.
pub fn chip_in(label: &str, current: bool, cursor: bool, focused: bool, hue: Style) -> Span<'static> {
    let style = match (cursor, focused) {
        (true, true) => selected(),
        (true, false) => hue.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        _ if current => hue,
        _ => dim(),
    };
    Span::styled(format!(" {}{label} ", if current { CURRENT } else { " " }), style)
}

/// A key as drawn in a hint: words become the symbols Mac keyboards print, so a footer scans by shape.
/// Key names the way the keyboard in front of you labels them, decided at compile time: on macOS its symbols
/// (⌃ control, ⌥ option, which is the Alt key terminals see, ⇧ ⎋ ⏎), elsewhere words (Ctrl+T, Alt+O, Esc).
const MAC: bool = cfg!(target_os = "macos");

/// One key combination written the neutral way ("ctrl+t", "alt+↑↓", "esc+esc", "shift+tab"), for this platform.
/// Words that are not keys ("/help", "type") pass through.
pub fn key_glyph(key: &str) -> String {
    glyph(key, MAC)
}

/// Test expectations are written with the Mac glyphs; this gives them as this platform draws them.
#[cfg(test)]
pub fn native(s: &str) -> String {
    [("⎋", "esc"), ("⏎", "enter"), ("␣", "space"), ("⇥", "tab")].iter().fold(s.to_string(), |s, (g, w)| s.replace(g, &key_glyph(w)))
}

fn glyph(key: &str, mac_keys: bool) -> String {
    let combo = key.contains('+');
    let word = |w: &str| -> String {
        let (mac, named) = match w {
            "esc" => ("⎋", true),
            "enter" => ("⏎", true),
            "space" => ("␣", true),
            "tab" => ("⇥", true),
            "ctrl" => ("⌃", true),
            "alt" => ("⌥", true),
            "shift" => ("⇧", true),
            _ => (w, false),
        };
        match (mac_keys, named || combo) {
            (true, _) => mac.into(),
            (false, true) => capital(w),
            (false, false) => w.into(),
        }
    };
    let parts: Vec<String> = key.split('+').map(|k| k.split('/').map(word).collect::<Vec<_>>().join("/")).collect();
    parts.join(if mac_keys { "" } else { "+" })
}

/// `t` → `T`, `end` → `End`.
fn capital(w: &str) -> String {
    let mut c = w.chars();
    c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
}

/// `text` with every key combination in it (`ctrl+o`, `alt+q`, `ctrl+shift+z`) written for this platform, as the
/// footers write them. For prose: notes, flashes, hints in the transcript.
pub fn keys(text: &str) -> String {
    keys_for(text, MAC)
}

fn keys_for(text: &str, mac_keys: bool) -> String {
    let key_char = |c: char| c.is_alphanumeric() || "+←→↑↓⌫".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let boundary = out.chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let starts = ["ctrl+", "alt+", "shift+"].iter().any(|m| rest.starts_with(m));
        if boundary && starts {
            let len = rest.find(|c: char| !key_char(c)).unwrap_or(rest.len());
            let combo = rest[..len].trim_end_matches('+');
            out += &glyph(combo, mac_keys);
            rest = &rest[combo.len()..];
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

/// One line of key hints, always in this shape: `key action · key action`. Keys are bold, actions dim.
pub fn hints(items: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, action)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.push(Span::styled(key_glyph(key), accent().add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {action}"), dim()));
    }
    Line::from(spans)
}

/// Make a hint line from [`hints`] fit `width` columns: middle hints are dropped first, so the first
/// (what you most likely want) and the last (how to get out, almost always Esc) survive.
pub fn fit_hints(line: Line<'static>, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let total = |l: &Line| l.spans.iter().map(|s| s.content.width()).sum::<usize>();
    if total(&line) <= width {
        return line;
    }
    // hints() lays spans out as: lead, key, action, then (sep, key, action) per further item
    let mut items: Vec<(String, String)> = Vec::new();
    let mut it = line.spans.iter().skip(1).peekable();
    while let Some(s) = it.next() {
        if s.content == " · " {
            continue;
        }
        if let Some(action) = it.next() {
            items.push((s.content.to_string(), action.content.trim_start().to_string()));
        }
    }
    let build = |items: &[(String, String)]| {
        let refs: Vec<(&str, &str)> = items.iter().map(|(k, a)| (k.as_str(), a.as_str())).collect();
        hints(&refs)
    };
    while items.len() > 2 && total(&build(&items)) > width {
        items.remove(items.len() - 2);
    }
    build(&items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_background_reply_says_light_or_dark() {
        assert_eq!(background_is_light("\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?62;c"), Some(true));
        assert_eq!(background_is_light("\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"), Some(false));
        assert_eq!(background_is_light("\x1b]11;rgb:fd/f6/e3\x07"), Some(true), "two-digit channels (solarized light)");
        assert_eq!(background_is_light("\x1b[?62;c"), None, "no OSC 11 answer");
    }

    #[test]
    fn a_band_sets_its_text_colour_so_it_reads_in_either_mode_and_the_setting_overrides_the_terminal() {
        for (mode, detected) in [("auto", false), ("auto", true), ("dark", true), ("light", false)] {
            set_mode(mode);
            set_light(detected);
            let light = mode == "light" || (mode == "auto" && detected);
            for band in [user_band(), added_line(), removed_line()] {
                assert!(band.bg.is_some() && band.fg.is_some(), "{mode}/{detected}: a band without its own text colour turns invisible after a switch");
                assert_eq!(band.fg, Some(Color::Indexed(if light { 235 } else { 252 })), "{mode}/{detected}");
            }
        }
        set_mode("auto");
        set_light(false);
    }

    #[test]
    fn keys_are_named_the_way_this_platform_labels_them() {
        let text = "click or ctrl+o to expand · alt+q to manage (ctrl+shift+z redoes), ctrl+end; esc to interrupt; Bashctrl+x";
        assert_eq!(keys_for(text, true), "click or ⌃o to expand · ⌥q to manage (⌃⇧z redoes), ⌃end; esc to interrupt; Bashctrl+x");
        assert_eq!(keys_for(text, false), "click or Ctrl+O to expand · Alt+Q to manage (Ctrl+Shift+Z redoes), Ctrl+End; esc to interrupt; Bashctrl+x");
        let combos = ["esc+esc", "shift+tab", "alt+↑↓", "/help", "type", "x"];
        assert_eq!(combos.map(|k| glyph(k, true)), ["⎋⎋", "⇧⇥", "⌥↑↓", "/help", "type", "x"]);
        assert_eq!(combos.map(|k| glyph(k, false)), ["Esc+Esc", "Shift+Tab", "Alt+↑↓", "/help", "type", "x"], "words that are not keys pass through");
        assert_eq!(keys("no keys here"), "no keys here");
        assert_eq!(MAC, cfg!(target_os = "macos"));
    }

    #[test]
    fn every_glyph_is_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        for g in [ON, OFF, PART, BRIEF, ALLOW, ASK, DENY, CLOSED, OPEN, CURRENT, CURSOR, "⌂", "▤", "◇", "◦", "◐", "◓", "◑", "◒", "◆", "⎋", "⏎", "␣", "⇥", "⌃", "⌥", "⇧", "⏵", "◷", "≡", "±", "⌕", "◎", "◈", "✦"] {
            assert_eq!(g.width(), 1, "{g} would shift columns");
        }
    }

    #[test]
    fn narrow_footers_keep_the_first_and_last_hint() {
        let full = hints(&[("↑↓", "move"), ("enter", "choose"), ("1-9", "pick"), ("esc", "decline")]);
        let text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(text(&fit_hints(full.clone(), 80)), native(" ↑↓ move · ⏎ choose · 1-9 pick · ⎋ decline"));
        assert_eq!(text(&fit_hints(full.clone(), 38)), native(" ↑↓ move · ⏎ choose · ⎋ decline"));
        assert_eq!(text(&fit_hints(full.clone(), 20)), native(" ↑↓ move · ⎋ decline"));
        assert_eq!(text(&fit_hints(full, 5)), native(" ↑↓ move · ⎋ decline"), "never fewer than two; the renderer clips the rest");
    }

    /// The consistency rule, enforced: surfaces name roles ("selected", "dim", "attention"), never colours.
    /// Only the theme itself, the ANSI translator for your status line, and the image placeholder (which
    /// encodes an id in a colour) may use raw ones.
    #[test]
    fn surfaces_use_theme_roles_not_raw_colours() {
        let sources = [
            ("ui.rs", include_str!("ui.rs")),
            ("explorer.rs", include_str!("explorer.rs")),
            ("prompt.rs", include_str!("prompt.rs")),
            ("markdown.rs", include_str!("markdown.rs")),
            ("main.rs", include_str!("main.rs")),
            ("session.rs", include_str!("session.rs")),
            ("panels.rs", include_str!("panels.rs")),
            ("pane_host.rs", include_str!("pane_host.rs")),
            ("commands.rs", include_str!("commands.rs")),
        ];
        let banned = ["Color::", ".dark_gray()", ".yellow()", ".cyan()", ".green()", ".red()", ".magenta()", ".blue()", ".white()", ".reversed()"];
        for (name, src) in sources {
            for (n, line) in src.lines().enumerate() {
                let code = line.split("//").next().unwrap_or_default();
                if let Some(b) = banned.iter().find(|b| code.contains(**b)) {
                    panic!("{name}:{} uses {b}: use a role from theme.rs instead\n  {}", n + 1, line.trim());
                }
            }
        }
    }

    #[test]
    fn hints_have_one_shape() {
        let l = hints(&[("↑↓", "move"), ("enter", "choose"), ("esc", "back")]);
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, native(" ↑↓ move · ⏎ choose · ⎋ back"));
        assert!(l.spans[1].style.add_modifier.contains(Modifier::BOLD), "keys are bold");
        assert_eq!(l.spans[2].style.fg, Some(Color::DarkGray), "actions recede");
    }
}
