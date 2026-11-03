//! The one place panels are drawn. Every interactive surface (permission and question prompts, `/command`
//! and `@file` suggestions, the model picker, the session explorer, config) is a [`View`], content only,
//! and [`render`] puts it in the same place with the same frame: anchored at the bottom of the screen,
//! directly above the input, a titled border, a body that scrolls to keep the cursor row in view, optional
//! pinned lines above and below it, an optional detail pane on wide screens, and a one-line key-hint footer.
//! Surfaces never choose position, border or footer layout themselves; that is what keeps them consistent.

use crate::theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// Where a panel's list was drawn, so a mouse click can be turned into cursor moves.
#[derive(Clone, Copy)]
pub struct ListHit {
    /// Screen area of the body rows.
    pub body: Rect,
    /// Index of the first body row on screen.
    pub first: usize,
    pub len: usize,
    /// The row the cursor is on.
    pub cursor: usize,
    /// The input box keeps the keyboard (suggestions): activating a row completes it.
    pub input_focus: bool,
}

impl ListHit {
    /// The list row at a screen position, if there is one.
    pub fn row_at(&self, col: u16, row: u16) -> Option<usize> {
        let inside = col >= self.body.x && col < self.body.right() && row >= self.body.y && row < self.body.bottom();
        Some(self.first + (row - self.body.y) as usize).filter(|r| inside && *r < self.len)
    }
}

/// Detail panes appear only when the panel is at least this wide.
pub const SIDE_MIN_WIDTH: u16 = 90;
/// Rows the rest of the screen needs: tab strip, input box (rule + a line), status line, and two rows of
/// transcript so the conversation never vanishes behind a panel.
const RESERVED: u16 = 1 + 2 + 1 + 2;

/// What a surface wants drawn. Build one with [`View::new`] and the `with_*` methods.
pub struct View {
    pub title: String,
    /// Yellow frame, and the agent is waiting on this: see DESIGN.md.
    pub blocking: bool,
    /// The input box keeps the keyboard (suggestions): the terminal cursor stays there.
    pub input_focus: bool,
    /// Pinned above the body (a filter box, a tab strip).
    pub head: Vec<Line<'static>>,
    pub body: Vec<Line<'static>>,
    /// Index into `body` of the row the cursor is on; the body scrolls to keep it visible.
    pub focus: usize,
    /// Pinned below the body (an effort row, a help line).
    pub tail: Vec<Line<'static>>,
    /// Detail for the highlighted row, shown beside the body when there is room.
    pub side: Option<Vec<Line<'static>>>,
    pub footer: Line<'static>,
    /// Show at least this many body rows (browsers want room), and at most `max_body`.
    pub min_rows: usize,
    pub max_body: usize,
}

impl View {
    pub fn new(title: impl Into<String>, body: Vec<Line<'static>>, focus: usize, footer: Line<'static>) -> View {
        View {
            title: title.into(),
            blocking: false,
            input_focus: false,
            head: vec![],
            body,
            focus,
            tail: vec![],
            side: None,
            footer,
            min_rows: 0,
            max_body: usize::MAX,
        }
    }
    pub fn blocking(mut self) -> View {
        self.blocking = true;
        self
    }
    pub fn input_focus(mut self) -> View {
        self.input_focus = true;
        self
    }
    pub fn with_head(mut self, head: Vec<Line<'static>>) -> View {
        self.head = head;
        self
    }
    pub fn with_tail(mut self, tail: Vec<Line<'static>>) -> View {
        self.tail = tail;
        self
    }
    pub fn with_side(mut self, side: Vec<Line<'static>>) -> View {
        self.side = Some(side);
        self
    }
    pub fn rows(mut self, min: usize, max: usize) -> View {
        self.min_rows = min;
        self.max_body = max;
        self
    }
}

/// Move a list cursor one row up or down, wrapping at the ends. `n` must be above 0.
pub fn cycle(i: usize, n: usize, up: bool) -> usize {
    if up { (i + n - 1) % n } else { (i + 1) % n }
}

/// A list row in the one shape every panel uses: cursor marker, then the content; the row under the
/// cursor is highlighted in full.
pub fn row(selected: bool, spans: Vec<Span<'static>>) -> Line<'static> {
    row_with(selected, selected, spans)
}

/// Like [`row`] but the marker and the highlight can differ: a row being edited keeps the cursor marker
/// without being drawn in reverse video.
pub fn row_with(marker: bool, highlight: bool, mut spans: Vec<Span<'static>>) -> Line<'static> {
    if highlight {
        for s in &mut spans {
            s.style = s.style.patch(theme::selected());
        }
    }
    let marker = if marker { format!("{} ", theme::CURSOR) } else { "  ".into() };
    spans.insert(0, Span::styled(marker, theme::accent()));
    Line::from(spans)
}

/// Rows the panel takes, borders included: what it asks for, but never so much that the rest of the screen
/// ([`RESERVED`]) is squeezed out.
pub fn height(v: &View, screen_h: u16) -> u16 {
    let body = v.body.len().max(v.min_rows).min(v.max_body);
    let want = (v.head.len() + body + v.tail.len() + 1 + 2) as u16;
    want.min(screen_h.saturating_sub(RESERVED)).max(4.min(screen_h))
}

/// First body row to draw so that `focus` is in view, centred when the list is longer than the window.
fn first_row(len: usize, window: usize, focus: usize) -> usize {
    if len <= window { 0 } else { focus.saturating_sub(window / 2).min(len - window) }
}

/// Draw `v` filling `area` (full width, directly above the input).
pub fn render(f: &mut Frame, area: Rect, v: View) -> Option<ListHit> {
    if area.height < 3 {
        return None;
    }
    f.render_widget(Clear, area);
    let block = theme::frame(&v.title, v.blocking);
    // one column of air inside the frame, so text never touches the border
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    Some(fill(f, area.width, inner, v))
}

/// Draw `v` as the whole screen: no frame, the title on the first row, the key hints on the last. Same body,
/// pinned lines, detail pane and footer as [`render`]; for browsers that deserve the full terminal.
pub fn render_full(f: &mut Frame, area: Rect, v: View) -> ListHit {
    f.render_widget(Clear, area);
    let [title, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    f.render_widget(Paragraph::new(Line::styled(format!(" {}", v.title), theme::bold())), title);
    fill(f, area.width, rest.inner(Margin::new(1, 0)), v)
}

fn fill(f: &mut Frame, width: u16, inner: Rect, v: View) -> ListHit {
    let [content, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let mut hints = theme::fit_hints(v.footer, footer.width as usize + 1);
    if !hints.spans.is_empty() {
        hints.spans.remove(0); // the margin already indents it
    }
    f.render_widget(Paragraph::new(hints), footer);

    let (main, side) = match v.side.as_ref().filter(|_| width >= SIDE_MIN_WIDTH) {
        Some(_) => {
            let [m, s] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(content);
            (m, Some(s))
        }
        None => (content, None),
    };
    let [head, body, tail] = Layout::vertical([
        Constraint::Length((v.head.len() as u16).min(main.height)),
        Constraint::Min(0),
        Constraint::Length((v.tail.len() as u16).min(main.height.saturating_sub(v.head.len() as u16 + 1))),
    ])
    .areas(main);
    f.render_widget(Paragraph::new(v.head), head);
    let skip = first_row(v.body.len(), body.height as usize, v.focus);
    let hit = ListHit { body, first: skip, len: v.body.len(), cursor: v.focus, input_focus: v.input_focus };
    f.render_widget(Paragraph::new(v.body.into_iter().skip(skip).collect::<Vec<_>>()), body);
    f.render_widget(Paragraph::new(v.tail), tail);
    if let (Some(lines), Some(side)) = (v.side, side) {
        f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::LEFT).border_style(theme::dim())), side);
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn lines(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::raw(format!("row {i}"))).collect()
    }
    fn draw(v: View, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| {
            let area = Rect::new(0, h - height(&v, h), w, height(&v, h));
            render(f, area, v);
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn every_view_gets_the_same_frame_title_and_footer() {
        let rows = draw(View::new("Things", lines(2), 0, theme::hints(&[("esc", "close")])), 40, 12);
        let bottom = rows.iter().rposition(|r| r.starts_with('└')).expect("bottom border");
        assert_eq!(bottom, 11, "anchored to the bottom edge");
        assert!(rows.iter().any(|r| r.starts_with("┌ Things ")), "{rows:?}");
        assert_eq!(rows[bottom - 1], format!("│ ⎋ close{}│", " ".repeat(40 - 2 - 8)), "footer is the last row inside the frame");
    }

    #[test]
    fn the_body_scrolls_to_keep_the_cursor_row_visible_and_pinned_lines_stay() {
        let v = View::new("Long", lines(50), 40, theme::hints(&[("esc", "close")])).with_head(vec![Line::raw("HEAD")]).with_tail(vec![Line::raw("TAIL")]).rows(10, 10);
        let rows = draw(v, 40, 24);
        assert!(rows.iter().any(|r| r.contains("row 40")), "focused row in view");
        assert!(!rows.iter().any(|r| r.contains("row 0")), "and the top scrolled away");
        let head = rows.iter().position(|r| r.contains("HEAD")).unwrap();
        let tail = rows.iter().position(|r| r.contains("TAIL")).unwrap();
        assert!(tail > head + 10, "pinned head above the scrolling body, tail below it");
    }

    #[test]
    fn height_is_what_is_asked_within_limits() {
        let v = |n, min, max| View::new("x", lines(n), 0, Line::raw("")).rows(min, max);
        assert_eq!(height(&v(3, 0, usize::MAX), 40), 3 + 1 + 2, "body + footer + borders");
        assert_eq!(height(&v(3, 12, usize::MAX), 40), 12 + 3, "browsers ask for room");
        assert_eq!(height(&v(60, 0, 8), 40), 8 + 3, "suggestions cap their rows");
        assert_eq!(height(&v(500, 0, usize::MAX), 30), 30 - RESERVED, "the rest of the screen keeps its rows");
    }

    #[test]
    fn the_detail_pane_needs_a_wide_screen() {
        let mk = || View::new("D", lines(2), 0, Line::raw("")).with_side(vec![Line::raw("DETAIL")]);
        assert!(!draw(mk(), 60, 12).iter().any(|r| r.contains("DETAIL")));
        assert!(draw(mk(), 120, 12).iter().any(|r| r.contains("DETAIL")));
    }

    #[test]
    fn full_screen_has_no_frame_title_first_hints_last() {
        let v = View::new("Everything", lines(3), 0, theme::hints(&[("esc", "close")])).with_side(vec![Line::raw("DETAIL")]);
        let mut t = Terminal::new(TestBackend::new(120, 10)).unwrap();
        t.draw(|f| { let _ = render_full(f, f.area(), v); }).unwrap();
        let b = t.backend().buffer().clone();
        let row = |y: u16| (0..120).map(|x| b[(x, y)].symbol().to_string()).collect::<String>();
        assert!(row(0).starts_with(" Everything") && row(9).contains("⎋ close"));
        assert!((0..10).all(|y| !row(y).contains('┌') && !row(y).contains('│') || row(y).contains("DETAIL") || !row(y).starts_with("│")));
        assert!(row(1).contains("row 0") && (0..10).any(|y| row(y).contains("DETAIL")), "body and the wide detail pane use the screen");
    }

    #[test]
    fn rows_have_one_shape() {
        let a = row(true, vec![Span::raw("picked")]);
        let b = row(false, vec![Span::raw("other")]);
        assert_eq!((a.spans[0].content.as_ref(), b.spans[0].content.as_ref()), ("❯ ", "  "));
        assert_eq!(a.spans[1].style, theme::selected());
        assert_ne!(b.spans[1].style, theme::selected());
    }
}
