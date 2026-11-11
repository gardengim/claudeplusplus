//! How "working" looks, in one place: the spinner, the bars that say how far something is, and the glint on
//! the word beside the spinner. A surface that shows work asks here instead of drawing its own.
//!
//! All three come from `config.json` as plain strings, so what you type is what is drawn:
//! - `spinner`: a sprite sheet. Every character is one frame (`"⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"`); if the value has spaces, each
//!   space-separated word is a frame (`"[   ] [=  ] [== ]"`). `none` hides it. Frames are padded to one width so
//!   the text beside the spinner never moves.
//! - `progress`: two characters, filled then empty (`"█░"`). `none` hides the bar and leaves the numbers.
//! - `motion`: `full` (spinner and a glint sweeping the verb), `calm` (spinner only), `off` (nothing moves, and
//!   the screen is redrawn once a second instead of ~8 times).
//!
//! A value that does not parse falls back to the default rather than breaking the screen. The look is built
//! once and swapped whole, so a frame reads it with one lock and no allocation beyond the strings it draws.

use ratatui::style::Style;
use ratatui::text::Span;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How long one spinner frame is shown.
pub const FRAME_MS: u64 = 120;
/// How long the working glyph in tab strips and lists holds one quarter turn.
const PHASE_MS: u64 = FRAME_MS * 2;
pub const SPINNER: &str = "·✢✳∗✻✽✻∗✳✢";
pub const BAR: &str = "█░";
const MAX_FRAMES: usize = 32;
const MAX_FRAME_WIDTH: usize = 4;
const HIDDEN: &str = "none";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    Full,
    Calm,
    Off,
}

impl Motion {
    fn parse(s: &str) -> Option<Motion> {
        Some(match s {
            "full" => Motion::Full,
            "calm" => Motion::Calm,
            "off" => Motion::Off,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Motion::Full => "full",
            Motion::Calm => "calm",
            Motion::Off => "off",
        }
    }
}

pub struct Look {
    /// The values as saved (the defaults where a value did not parse), shown back in `/config`.
    spinner: String,
    progress: String,
    motion: Motion,
    /// Empty: no spinner.
    frames: Vec<String>,
    /// Filled and empty cell; None: no bars.
    cells: Option<[String; 2]>,
}

/// Split into cells: each character starts one, zero-width ones (marks, variation selectors) stay with the last.
fn cells(s: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for c in s.chars().filter(|c| !c.is_control()) {
        match out.last_mut() {
            Some(last) if c.width() == Some(0) => last.push(c),
            _ => out.push(c.to_string()),
        }
    }
    out
}

/// `None`: not usable. `Some(vec![])`: hidden.
fn parse_frames(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    if s == HIDDEN {
        return Some(vec![]);
    }
    let mut frames = if s.contains(char::is_whitespace) {
        s.split_whitespace().map(|w| w.chars().filter(|c| !c.is_control()).collect::<String>()).collect()
    } else {
        cells(s)
    };
    frames.retain(|f: &String| !f.is_empty());
    let widest = frames.iter().map(|f| f.width()).max()?;
    if frames.len() > MAX_FRAMES || widest > MAX_FRAME_WIDTH || frames.iter().any(|f| f.width() == 0) {
        return None;
    }
    Some(frames.into_iter().map(|f| format!("{f}{}", " ".repeat(widest - f.width()))).collect())
}

/// `None`: not usable. `Some(None)`: hidden.
fn parse_cells(s: &str) -> Option<Option<[String; 2]>> {
    let s = s.trim();
    if s == HIDDEN {
        return Some(None);
    }
    let [on, off]: [String; 2] = cells(s).try_into().ok()?;
    (on.width() == 1 && off.width() == 1).then_some(Some([on, off]))
}

impl Look {
    fn new(spinner: &str, progress: &str, motion: &str) -> Look {
        let (frames, spinner) = match parse_frames(spinner) {
            Some(f) if !spinner.is_empty() => (f, spinner.trim()),
            _ => (parse_frames(SPINNER).unwrap_or_default(), SPINNER),
        };
        let (cells, progress) = match parse_cells(progress) {
            Some(c) if !progress.is_empty() => (c, progress.trim()),
            _ => (parse_cells(BAR).unwrap_or_default(), BAR),
        };
        Look { spinner: spinner.into(), progress: progress.into(), motion: Motion::parse(motion).unwrap_or(Motion::Calm), frames, cells }
    }

    /// The saved value of `spinner`, `progress` or `motion`.
    pub fn raw(&self, key: &str) -> &str {
        match key {
            "spinner" => &self.spinner,
            "progress" => &self.progress,
            _ => self.motion.name(),
        }
    }

    pub fn glint(&self) -> bool {
        self.motion == Motion::Full
    }

    /// Does anything on screen change with the clock? If not, nothing needs redrawing between events.
    fn ticks(&self) -> bool {
        self.motion != Motion::Off && (self.frames.len() > 1 || self.glint())
    }

    /// The spinner frame `el` into a turn; None when hidden. Still (one frame, or motion off): the middle one.
    pub fn frame(&self, el: Duration) -> Option<&str> {
        let n = self.frames.len();
        let i = if self.motion == Motion::Off || n == 1 { n.saturating_sub(1) / 2 } else { (el.as_millis() as u64 / FRAME_MS) as usize % n.max(1) };
        self.frames.get(i).map(String::as_str)
    }

    /// Milliseconds the event loop may sleep while the tab in front works: to just after the next frame is due
    /// (so every frame shows for the same time), or the next second when the timer is all that moves.
    pub fn wait_ms(&self, el: Duration) -> u64 {
        let step = if self.ticks() { FRAME_MS } else { 1000 };
        step - el.as_millis() as u64 % step + 2
    }

    /// The same while only other tabs work: their marks turn on the shared clock.
    pub fn background_wait_ms(&self) -> u64 {
        if self.ticks() { PHASE_MS - clock_ms() % PHASE_MS + 2 } else { 1000 }
    }

    /// `text` in `base`, with a three-character glint in `glint` sweeping across it (then a pause).
    pub fn shimmer(&self, text: &str, base: Style, glint: Style, el: Duration) -> Vec<Span<'static>> {
        if !self.glint() {
            return vec![Span::styled(text.to_string(), base)];
        }
        let n = text.chars().count();
        let pos = (el.as_millis() as u64 / FRAME_MS) as usize % (n + 8);
        let at = |k: usize| text.char_indices().nth(k.min(n)).map_or(text.len(), |(i, _)| i);
        let (x, y) = (at(pos.saturating_sub(3)), at(pos));
        [(&text[..x], base), (&text[x..y], glint), (&text[y..], base)].into_iter().filter(|(t, _)| !t.is_empty()).map(|(t, s)| Span::styled(t.to_string(), s)).collect()
    }

    /// A bar `width` cells wide, `frac` full: the filled part and the empty part (both empty when bars are hidden).
    pub fn bar(&self, frac: f64, width: usize) -> (String, String) {
        let Some([on, off]) = &self.cells else { return Default::default() };
        let full = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
        (on.repeat(full), off.repeat(width - full))
    }
}

static LOOK: RwLock<Option<Arc<Look>>> = RwLock::new(None);

pub fn look() -> Arc<Look> {
    if let Some(l) = LOOK.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return l.clone();
    }
    static DEFAULT: OnceLock<Arc<Look>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(Look::new("", "", ""))).clone()
}

/// Set all three from the saved values (empty or unusable: the default).
pub fn configure(spinner: &str, progress: &str, motion: &str) {
    *LOOK.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(Look::new(spinner, progress, motion)));
}

/// Change one of `spinner`, `progress`, `motion`.
pub fn set(key: &str, value: &str) {
    let l = look();
    let get = |k: &str| if k == key { value.to_string() } else { l.raw(k).to_string() };
    configure(&get("spinner"), &get("progress"), &get("motion"));
}

/// A bar as one string, for lines that draw it in one style.
pub fn bar(frac: f64, width: usize) -> String {
    let (on, off) = look().bar(frac, width);
    on + &off
}

/// `▰▰▱▱ 2/4`, or just `2/4` when bars are hidden.
pub fn count_bar(done: usize, total: usize, width: usize) -> String {
    let b = bar(done as f64 / total.max(1) as f64, width);
    if b.is_empty() { format!("{done}/{total}") } else { format!("{b} {done}/{total}") }
}

// The mark for "working" in tab strips and lists turns a quarter at a time while a tab of ours works; the event
// loop sets the phase, so nothing else is redrawn faster than it already is.
static PHASE: AtomicU8 = AtomicU8::new(0);
const TURN: [&str; 4] = ["◐", "◓", "◑", "◒"];

fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Called by the event loop before each frame; `busy`: some tab of ours is working.
pub fn advance(busy: bool) {
    let phase = if busy && look().motion != Motion::Off { (clock_ms() / PHASE_MS % 4) as u8 } else { 0 };
    PHASE.store(phase, Ordering::Relaxed);
}

pub fn working_glyph() -> &'static str {
    TURN[PHASE.load(Ordering::Relaxed) as usize % 4]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_sprite_sheet_is_read_as_typed_and_padded_to_one_width() {
        let l = Look::new("⠋⠙⠹", "", "");
        assert_eq!((l.frames.len(), l.frame(ms(0)), l.frame(ms(FRAME_MS)), l.frame(ms(3 * FRAME_MS))), (3, Some("⠋"), Some("⠙"), Some("⠋")));
        let l = Look::new("a bcd", "", "");
        assert_eq!(l.frames, ["a  ", "bcd"], "words are frames, padded so the text beside never moves");
        let l = Look::new("✳\u{fe0f}x", "", "");
        assert_eq!(l.frames.len(), 2, "a variation selector stays with its character");
    }

    #[test]
    fn unusable_values_fall_back_and_none_hides() {
        let d = Look::new("", "", "");
        assert_eq!((d.raw("spinner"), d.raw("progress"), d.raw("motion")), (SPINNER, BAR, "calm"));
        let many = (0..40).map(|i| format!("{i}")).collect::<Vec<_>>().join(" ");
        for bad in ["toolongframe x", many.as_str()] {
            assert_eq!(Look::new(bad, "", "sideways").raw("spinner"), SPINNER, "{bad}");
        }
        assert!(!Look::new("\u{1b}[31m", "", "").frames.concat().contains('\u{1b}'), "no control characters reach the screen");
        assert_eq!((Look::new("#", "#", "").raw("progress"), Look::new("", "#-x", "").raw("progress")), (BAR, BAR), "a bar is exactly two characters");
        let hidden = Look::new("none", "none", "");
        assert_eq!((hidden.frame(ms(0)), hidden.bar(0.5, 4)), (None, Default::default()));
    }

    #[test]
    fn bars_fill_in_the_given_characters() {
        let l = Look::new("", "#-", "");
        assert_eq!(l.bar(0.5, 4), ("##".to_string(), "--".to_string()));
        assert_eq!(l.bar(7.0, 3), ("###".to_string(), String::new()), "over full stays in its width");
        assert_eq!(l.bar(-1.0, 3), (String::new(), "---".to_string()));
    }

    #[test]
    fn nothing_moves_when_motion_is_off_and_the_loop_sleeps_to_the_second() {
        let off = Look::new("", "", "off");
        assert_eq!(off.frame(ms(0)), off.frame(ms(5000)));
        assert_eq!((off.wait_ms(ms(1500)), off.background_wait_ms()), (502, 1000));
        let calm = Look::new("", "", "calm");
        assert_eq!(calm.wait_ms(ms(1500)), FRAME_MS - 1500 % FRAME_MS + 2);
        assert!(Look::new("x", "", "calm").wait_ms(ms(1500)) > FRAME_MS, "one frame is as still as none");
        assert!(Look::new("x", "", "full").wait_ms(ms(1500)) <= FRAME_MS + 2, "but the glint still moves");
    }

    #[test]
    fn the_glint_never_changes_the_text() {
        let full = Look::new("", "", "full");
        let (a, b) = (Style::new(), Style::new().bold());
        for t in 0..40 {
            let spans = full.shimmer("Calling Bash…", a, b, ms(t * FRAME_MS));
            assert_eq!(spans.iter().map(|s| s.content.as_ref()).collect::<String>(), "Calling Bash…");
            assert!(spans.len() <= 3);
        }
        assert!(full.shimmer("Calling Bash…", a, b, ms(5 * FRAME_MS)).iter().any(|s| s.style == b));
        assert_eq!(Look::new("", "", "calm").shimmer("x", a, b, ms(0)).len(), 1);
    }

    #[test]
    fn every_choice_offered_in_config_is_usable_and_the_marks_are_one_column() {
        for item in crate::settings::ITEMS.iter().filter(|i| ["spinner", "progress", "motion"].contains(&i.key)) {
            for (value, _) in item.choices {
                assert_eq!(Look::new(
                    if item.key == "spinner" { value } else { "" },
                    if item.key == "progress" { value } else { "" },
                    if item.key == "motion" { value } else { "" },
                ).raw(item.key), *value, "{} = {value}", item.key);
            }
        }
        for g in TURN {
            assert_eq!(g.width(), 1);
        }
    }
}
