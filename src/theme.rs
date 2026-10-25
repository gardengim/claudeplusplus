//! The look of claude++, by meaning. Surfaces ask for a role ("this row is selected", "the agent is waiting
//! on you") and never name a colour, so the whole UI can be re-skinned, or checked for consistency, in this
//! one file. See DESIGN.md for the rules these encode.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

/// You, and anything you control: your messages, the cursor, the focused tab.
pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
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

/// The row the cursor is on.
pub fn selected() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
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
        _ => Color::Indexed(141),
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
    let style = if cursor { selected() } else if current { hue } else { dim() };
    Span::styled(format!(" {}{label} ", if current { CURRENT } else { " " }), style)
}

/// A key as drawn in a hint: words become the symbols Mac keyboards print, so a footer scans by shape.
fn key_glyph(key: &str) -> String {
    key.split('+')
        .map(|k| {
            k.split('/')
                .map(|w| match w {
                    "esc" => "⎋",
                    "enter" => "⏎",
                    "space" => "␣",
                    "tab" => "⇥",
                    "ctrl" => "⌃",
                    "alt" => "⌥",
                    "shift" => "⇧",
                    w => w,
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A bordered box. `blocking` boxes are the agent asking you something (yellow); the rest are things you
/// opened yourself (cyan). The colour tells you who is waiting on whom.
pub fn frame(title: &str, blocking: bool) -> Block<'static> {
    let hue = if blocking { attention() } else { accent() };
    Block::bordered().title(Span::styled(format!(" {title} "), hue.add_modifier(Modifier::BOLD))).border_style(hue)
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
    fn every_glyph_is_one_column_wide() {
        use unicode_width::UnicodeWidthStr;
        for g in [ON, OFF, PART, BRIEF, ALLOW, ASK, DENY, CLOSED, OPEN, CURRENT, CURSOR, "⌂", "▤", "◇", "◦", "◐", "◆", "⎋", "⏎", "␣", "⇥", "⌃", "⌥", "⇧", "⏵", "◷", "≡", "±", "⌕", "◎", "◈", "✦"] {
            assert_eq!(g.width(), 1, "{g} would shift columns");
        }
    }

    #[test]
    fn narrow_footers_keep_the_first_and_last_hint() {
        let full = hints(&[("↑↓", "move"), ("enter", "choose"), ("1-9", "pick"), ("esc", "decline")]);
        let text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(text(&fit_hints(full.clone(), 80)), " ↑↓ move · ⏎ choose · 1-9 pick · ⎋ decline");
        assert_eq!(text(&fit_hints(full.clone(), 38)), " ↑↓ move · ⏎ choose · ⎋ decline");
        assert_eq!(text(&fit_hints(full.clone(), 20)), " ↑↓ move · ⎋ decline");
        assert_eq!(text(&fit_hints(full, 5)), " ↑↓ move · ⎋ decline", "never fewer than two; the renderer clips the rest");
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
        assert_eq!(text, " ↑↓ move · ⏎ choose · ⎋ back");
        assert!(l.spans[1].style.add_modifier.contains(Modifier::BOLD), "keys are bold");
        assert_eq!(l.spans[2].style.fg, Some(Color::DarkGray), "actions recede");
    }
}
