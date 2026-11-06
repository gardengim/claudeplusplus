//! Rendering. Each transcript entry is wrapped once per width and cached, and only
//! the visible window of lines is handed to ratatui, so redraw cost stays flat as
//! the conversation grows.

use crate::panel::{self, View};
use crate::{App, instructions, optimize, permissions, settings, tabcfg, theme, usage};
use crate::session::{Entry, Item, Part, Pending, Session};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SPINNER: [&str; 10] = ["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const COLLAPSED_LINES: usize = 4;

/// The docked list needs a screen at least this wide, and takes this many columns.
const DOCK_MIN_SCREEN: u16 = 100;
const DOCK_W: u16 = 30;

/// Where the last frame drew things a click can hit.
#[derive(Clone, Default)]
pub struct Hits {
    /// The tab strip's row, and each tab's column range.
    pub tab_y: u16,
    pub tabs: Vec<(u16, u16)>,
    /// The open panel's (or the full-screen list's) rows.
    pub list: Option<panel::ListHit>,
    /// The docked session column: its area and the rows in it.
    pub dock: Option<(Rect, panel::ListHit)>,
    /// Tool calls in the transcript: first row, last row (exclusive) and the item, for a click to fold.
    pub calls: Vec<(u16, u16, usize)>,
    /// The strip of queued messages above the input.
    pub queue: Option<Rect>,
    /// The conversation's area, where a drag selects text.
    pub body: Rect,
}

/// Rows the editor gets in the message box's place: a third of the screen, 5 to 14.
pub fn pane_rows(screen_h: u16) -> u16 {
    (screen_h / 3).clamp(5, 14).min(screen_h.saturating_sub(6).max(1))
}

/// ctrl+g: the editor takes the whole screen (a hint row stays at the bottom), the conversation keeps updating behind it.
fn draw_editor_full(f: &mut Frame, app: &mut App) {
    let waiting = !app.sessions[app.active].pending.is_empty();
    let [body, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let Some(p) = app.pane.as_mut() else { return };
    p.resize(body.height, body.width);
    if let Some((x, y)) = p.render(f.buffer_mut(), body) {
        f.set_cursor_position(Position::new(x, y));
    }
    let hint = format!(" {} · quit it to put the text in the box · ctrl+\\ cancels", p.editor);
    let mut spans = vec![Span::styled(hint, theme::dim())];
    if waiting {
        spans.push(Span::styled(" · ◆ Claude is waiting for you", theme::attention()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), foot);
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits = Hits::default();
    if app.pane.as_ref().is_some_and(|p| p.kind == crate::pane::Use::Edit) {
        return draw_editor_full(f, app);
    }
    // The whole UI wears the colour being picked, or of the conversation being browsed, in real time.
    let accent = app.accent_color();
    theme::set_accent(accent.as_deref());
    // The full-screen session list replaces everything: no tabs, input or frame.
    let open = app.open_states();
    if let Some(ex) = app.explorer.as_mut() {
        ex.set_open(open);
        let area = f.area();
        let v = ex.view(area.width as usize);
        app.hits.list = Some(panel::render_full(f, area, v));
        return;
    }
    let mut area = f.area();
    // The docked session list takes a column on the left; everything else lays out in what remains.
    if area.width >= DOCK_MIN_SCREEN && app.dock.is_some() {
        let [dock, rest] = Layout::horizontal([Constraint::Length(DOCK_W), Constraint::Min(1)]).areas(area);
        let focused = app.dock_focus;
        let open = app.open_states();
        if let Some(d) = app.dock.as_mut() {
            d.set_open(open);
            let lines = d.dock_lines(DOCK_W as usize - 1, dock.height as usize, focused);
            f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::RIGHT).border_style(if focused { theme::accent() } else { theme::dim() })), dock);
            let (first, len, cursor) = d.dock_hit;
            // Row 0 of the column is its heading, so entries start one row down.
            let body = Rect::new(dock.x, dock.y + 1, dock.width.saturating_sub(1), dock.height.saturating_sub(1));
            app.hits.dock = Some((dock, panel::ListHit { body, first, len, cursor, input_focus: false }));
        }
        area = rest;
    }
    let input_w = area.width.saturating_sub(2).max(1) as usize;
    app.input_w = input_w;
    let (input_lines, cursor) = input_layout(&app.input, app.cursor, input_w);
    // Whatever panel is open (a prompt, a picker, config, suggestions...) is built as a `View`, sized and
    // drawn by the one shared panel renderer, at the bottom of the screen above the input.
    let view = surface(app, area.width as usize);
    let panel_h = view.as_ref().map_or(0, |v| panel::height(v, area.height));
    let input_keeps_focus = view.as_ref().is_none_or(|v| v.input_focus);
    let s = &app.sessions[app.active];
    let chars: Vec<char> = app.input.chars().collect();
    let sel = app.vim.on.then(|| app.vim.highlight(&chars, app.cursor)).flatten();
    let (input_rows, cursor_row) = input_rows(&input_lines, cursor.1, &s.attachments, &chars, sel);
    // The user's own status line (if configured and it has produced output) sits under the input.
    // Like Claude Code, it steps aside while a permission prompt or question is up, and on tiny terminals.
    let ext_h = if app.statusline_on && s.pending.is_empty() && area.height >= 14 {
        s.status_lines.len().min(crate::statusline::MAX_LINES) as u16
    } else {
        0
    };
    // While the agent works (and is not asking you anything), a spinner line sits above the input.
    let spin_h = u16::from(s.busy && s.pending.is_empty() && area.height >= 8);
    // Messages waiting for this turn to end sit between the spinner and the input.
    let queue_lines = if s.queue.is_empty() || app.queue_ui.is_some() || area.height < 12 { vec![] } else { crate::queue::strip(&s.queue, s.queue_hold, area.width as usize) };
    // The agent's open tasks stay in view under the spinner, as in Claude Code.
    let task_lines = if area.height < 16 || panel_h > 0 { vec![] } else { task_strip(&s.tasks, area.width as usize) };
    let [tabs, body, spin_area, task_area, queue_area, panel_area, input_area, ext_area, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(spin_h),
        Constraint::Length(task_lines.len() as u16),
        Constraint::Length(queue_lines.len() as u16),
        Constraint::Length(panel_h),
        Constraint::Length(if app.pane.is_some() { pane_rows(area.height) } else { input_rows.len().min((area.height / 2).max(8) as usize) as u16 } + 2),
        Constraint::Length(ext_h),
        Constraint::Length(1),
    ])
    .areas(area);
    if ext_h > 0 {
        f.render_widget(Paragraph::new(s.status_lines[..ext_h as usize].to_vec()), ext_area);
    }
    if !task_lines.is_empty() {
        f.render_widget(Paragraph::new(task_lines), task_area);
    }
    if !queue_lines.is_empty() {
        f.render_widget(Paragraph::new(queue_lines), queue_area);
        app.hits.queue = Some(queue_area);
    }
    if spin_h > 0 {
        f.render_widget(Paragraph::new(spinner_line(s, spin_area.width as usize)), spin_area);
    }

    f.render_widget(Paragraph::new(tab_line(app)), tabs);
    let mut x = tabs.x;
    app.hits.tab_y = tabs.y;
    for g in tab_groups(app) {
        let w: usize = g.iter().map(|s| s.content.width()).sum();
        app.hits.tabs.push((x, (x + w as u16).min(tabs.right())));
        x += w as u16;
    }
    let expanded = app.expanded;
    let cwd = app.cwd.clone();
    let peek = if app.dock_peek && app.dock_focus { app.dock.as_mut().map(|d| d.peek_lines(body.width as usize)) } else { None };
    match peek {
        Some(lines) => {
            let skip = lines.len().saturating_sub(body.height as usize);
            f.render_widget(Paragraph::new(lines).scroll((skip as u16, 0)), body);
        }
        None => app.hits.calls = transcript(f, &mut app.sessions[app.active], body, expanded, &cwd),
    }
    app.hits.body = body;
    select(f, app, body);
    if let Some(view) = view.filter(|_| panel_h > 0) {
        app.hits.list = panel::render(f, panel_area, view);
    }

    // Input mode editor, editing: the box is the editor.
    if app.pane.is_some() {
        let title = app.pane.as_ref().map_or(String::new(), |p| format!(" {} · quit it to send · ctrl+\\ cancels ", p.editor));
        let rule = Block::new().borders(Borders::TOP | Borders::BOTTOM).border_style(theme::accent()).title(Span::styled(title, theme::accent()));
        let inner = rule.inner(input_area);
        f.render_widget(rule, input_area);
        let keeps = input_keeps_focus && app.sessions[app.active].pending.is_empty();
        if let Some(p) = app.pane.as_mut() {
            p.resize(inner.height, inner.width);
            let at = p.render(f.buffer_mut(), inner);
            if let Some((x, y)) = at.filter(|_| keeps) {
                f.set_cursor_position(Position::new(x, y));
            }
        }
        f.render_widget(Paragraph::new(status_line(app, area.height)), status);
        return;
    }
    // Input box: a rule above and below plus the wrapped text, scrolled to keep the cursor visible.
    let visible = input_area.height.saturating_sub(2) as usize;
    let skip = (cursor_row + 1).saturating_sub(visible);
    let text: Vec<Line> = if app.input.is_empty() {
        vec![Line::from(vec![Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD)), Span::styled(app.placeholder(), theme::dim())])]
    } else {
        input_rows.into_iter().skip(skip).collect()
    };
    let mut rule = Block::new().borders(Borders::TOP | Borders::BOTTOM).border_style(theme::dim());
    if app.input.trim_start().starts_with('!') {
        rule = rule.title(Span::styled(" ! shell: runs here, no model turn; the output goes with your next message ", theme::tool_hue("Bash")));
    }
    if let Some(due) = app.sched_at {
        let now = std::time::SystemTime::now();
        rule = rule.title(Span::styled(format!(" scheduled for {} (in {}) · enter adds it · esc cancels ", crate::queue::clock(due), crate::queue::until(due, now)), theme::accent()));
    }
    if app.vim.on {
        let pending: String = if app.vim.pending() { " …".into() } else { String::new() };
        rule = rule.title(Line::from(Span::styled(format!(" {}{pending} ", app.vim.label()), theme::accent().add_modifier(Modifier::BOLD))).right_aligned());
    }
    f.render_widget(Paragraph::new(text).block(rule), input_area);
    // A panel that owns the keyboard also owns the cursor: leave it hidden.
    if input_keeps_focus {
        f.set_cursor_position(Position::new(input_area.x + 2 + cursor.0 as u16, input_area.y + 1 + cursor_row.saturating_sub(skip) as u16));
    }
    f.render_widget(Paragraph::new(status_line(app, area.height)), status);
}

/// The rows of `body` between two screen cells (x, y), as (y, first x, last x): the span of a drag.
fn sel_rows(body: Rect, a: (u16, u16), b: (u16, u16)) -> Vec<(u16, u16, u16)> {
    if body.is_empty() {
        return vec![];
    }
    let clamp = |p: (u16, u16)| (p.0.clamp(body.x, body.right() - 1), p.1.clamp(body.y, body.bottom() - 1));
    let (a, b) = (clamp(a), clamp(b));
    let (s, e) = if (a.1, a.0) <= (b.1, b.0) { (a, b) } else { (b, a) };
    (s.1..=e.1).map(|y| (y, if y == s.1 { s.0 } else { body.x }, if y == e.1 { e.0 } else { body.right() - 1 })).collect()
}

/// While a drag is on: remember what the conversation shows (a wide glyph's second cell is "") and draw the
/// dragged part reversed.
fn select(f: &mut Frame, app: &mut App, body: Rect) {
    let Some((a, b)) = app.drag else { return };
    let buf = f.buffer_mut();
    app.snap = (body.y..body.bottom())
        .map(|y| {
            let mut skip = 0;
            (body.x..body.right())
                .map(|x| {
                    let sym = buf.cell((x, y)).map_or(String::new(), |c| c.symbol().to_string());
                    if skip > 0 {
                        skip -= 1;
                        return String::new();
                    }
                    skip = sym.width().saturating_sub(1);
                    sym
                })
                .collect()
        })
        .collect();
    if a != b {
        for (y, x0, x1) in sel_rows(body, a, b) {
            for x in x0..=x1 {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

/// The text between two screen cells of the conversation, from the cells drawn last (`snap`).
pub fn selection_text(snap: &[Vec<String>], body: Rect, a: (u16, u16), b: (u16, u16)) -> String {
    let lines: Vec<String> = sel_rows(body, a, b)
        .into_iter()
        .map(|(y, x0, x1)| {
            let row = snap.get((y - body.y) as usize).map_or(&[][..], |r| &r[..]);
            row.get((x0 - body.x) as usize..=(x1 - body.x) as usize).unwrap_or_default().concat().trim_end().to_string()
        })
        .collect();
    lines.join("\n").trim_end().to_string()
}

/// The panel to show, if any. One at a time; the first that applies wins.
fn surface(app: &mut App, width: usize) -> Option<View> {
    if let Some(p) = app.sessions[app.active].pending.front() {
        return Some(prompt_view(p, width.saturating_sub(4), &app.cwd));
    }
    if let Some(sel) = app.color_picker {
        return Some(color_view(sel));
    }
    if app.model_picker.is_some() {
        return Some(model_view(app));
    }
    if let Some(p) = app.rewind {
        return Some(rewind_view(&app.sessions[app.active], p, width));
    }
    if let Some(ui) = &app.sched_ui {
        let s = &app.sessions[app.active];
        let q = ui.target.and_then(|n| s.queue.get(n));
        let gist = q.map(|q| crate::queue::gist(q, width.saturating_sub(16)));
        return Some(crate::schedule::view(ui, gist.as_deref(), q.is_some_and(|q| q.due.is_some()), std::time::SystemTime::now()));
    }
    if let Some(ui) = &app.queue_ui {
        let s = &app.sessions[app.active];
        return Some(crate::queue::view(ui, &s.queue, s.queue_hold, s.busy, width));
    }
    if let Some(ui) = &app.search {
        return Some(crate::search::view(ui));
    }
    if let Some(ui) = &app.lanes {
        let s = &app.sessions[app.active];
        return Some(crate::lanes::view(ui, &crate::lanes::collect(&s.items, &s.shells, s.cwd.as_deref().unwrap_or(&app.cwd)), width));
    }
    if let Some(ui) = &app.ctx_ui {
        return Some(crate::context::view(ui, &app.sessions[app.active].usage, width));
    }
    if app.usage_open {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        return Some(usage::view(&app.sessions[app.active].usage, width, now));
    }
    if let Some(ui) = &app.opt {
        return Some(optimize::view(ui, width));
    }
    if let Some(ui) = &app.perms {
        return Some(permissions::view(ui, width));
    }
    if let Some(ui) = &app.instr {
        return Some(instructions::view(ui, width));
    }
    if let Some(ui) = &app.tabcfg {
        return Some(tabcfg::view(ui, &app.sessions[app.active], width));
    }
    if let Some(ui) = &app.settings {
        return Some(settings::view(ui, width, |k| app.setting_value(k)));
    }
    if let Some(ui) = &app.wt_ui {
        let canon = |p: &str| std::fs::canonicalize(p).map_or(p.to_string(), |c| c.to_string_lossy().into_owned());
        let tabs: Vec<String> = app.sessions.iter().map(|s| canon(s.cwd.as_deref().unwrap_or(&app.cwd))).collect();
        let here = canon(app.cwd());
        return Some(crate::worktree::view(ui, &tabs, &here));
    }
    if let Some(ui) = &app.open_ui {
        return Some(crate::open::view(ui));
    }
    if let Some(ui) = &app.items_ui {
        return Some(crate::segments::view(ui, &app.line_items));
    }
    let rows = app.completions();
    (!rows.is_empty()).then(|| suggest_view(&rows, app.sel, width))
}

/// The `/color` choices, each name in its own colour; the UI behind already wears the highlighted one.
fn color_view(sel: usize) -> View {
    let body: Vec<Line> = App::color_choices().into_iter().enumerate().map(|(i, c)| panel::row(i == sel, vec![Span::styled(format!("{} ", theme::CURRENT), theme::tint(Some(c))), Span::raw(c)])).collect();
    let footer = theme::hints(&[("↑↓", "preview"), ("enter", "apply"), ("esc", "cancel")]);
    View::new("Colour", body, sel, footer)
}

/// The model list with the effort levels and fast mode of the highlighted model pinned beneath it.
fn model_view(app: &App) -> View {
    let Some(p) = app.model_picker else { return View::new("", vec![], 0, Line::raw("")) };
    let models = app.model_list();
    let s = &app.sessions[app.active];
    let cur = app.current_model();
    let mut body: Vec<Line> = models
        .iter()
        .enumerate()
        .map(|(i, m)| panel::row(i == p.sel, vec![Span::raw(format!("{} {:<24} {}", if cur == Some(i) { theme::CURRENT } else { " " }, m.name, m.desc))]))
        .collect();
    if body.is_empty() {
        body.push(Line::styled("loading models…", theme::dim()));
    }
    let mut tail = Vec::new();
    if let Some(m) = models.get(p.sel).filter(|m| !m.efforts.is_empty()) {
        let mut spans = vec![Span::raw("effort  ")];
        for (i, e) in m.efforts.iter().enumerate() {
            spans.push(theme::chip(e, *e == s.effort, i == p.effort, theme::bold()));
        }
        tail.push(Line::from(spans));
    }
    // Fast mode: Opus only, so the row says so instead of vanishing and shifting the layout.
    let cur_fast = s.fast_mode || s.fast_pref == Some(true);
    if models.get(p.sel).is_some_and(|m| m.fast) {
        let mut spans = vec![Span::raw("fast ↯  ")];
        for (on, label) in [(false, "off"), (true, "on")] {
            spans.push(theme::chip(label, cur_fast == on, p.fast == on, theme::bold()));
        }
        spans.push(Span::styled("  faster output, higher price per token", theme::dim()));
        tail.push(Line::from(spans));
    } else {
        tail.push(Line::styled("fast ↯  not available on this model (Opus only)", theme::dim()));
    }
    let footer = theme::hints(&[("↑↓", "model"), ("←→", "effort"), ("space", "fast"), ("enter", "apply"), ("esc", "cancel")]);
    View::new("Model, effort & fast mode", body, p.sel, footer).with_tail(tail)
}

/// Earlier messages to go back to, with what to restore pinned beneath.
fn rewind_view(s: &crate::session::Session, p: crate::RewindPicker, width: usize) -> View {
    let points = s.rewind_points();
    let sel = p.sel.min(points.len().saturating_sub(1));
    let body: Vec<Line> = points
        .iter()
        .enumerate()
        .map(|(i, (_, t))| panel::row(i == sel, vec![Span::raw(truncate(&t.split_whitespace().collect::<Vec<_>>().join(" "), width.saturating_sub(8)))]))
        .collect();
    let edited = points.get(sel).is_some_and(|(at, _)| s.edits_since(*at));
    let choices = crate::RewindPicker::choices(edited);
    let mut spans = vec![Span::raw("restore  ")];
    for (i, (label, ..)) in choices.iter().enumerate() {
        spans.push(theme::chip(label, false, i == p.restore.min(choices.len() - 1), theme::bold()));
    }
    if !edited {
        spans.push(Span::styled("  no files edited since", theme::dim()));
    }
    let keys: &[(&str, &str)] = if edited { &[("↑↓", "message"), ("←→", "restore"), ("enter", "rewind"), ("esc", "cancel")] } else { &[("↑↓", "message"), ("enter", "rewind"), ("esc", "cancel")] };
    View::new("Rewind to before…", body, sel, theme::hints(keys)).with_tail(vec![Line::from(spans)])
}

/// `/command` and `@file` suggestions: the input keeps the keyboard, the panel is just a list.
fn suggest_view(rows: &[crate::Completion], sel: usize, width: usize) -> View {
    let inner = width.saturating_sub(4);
    let sel = sel.min(rows.len().saturating_sub(1));
    let cap = if rows[0].prefix == '@' { inner.saturating_sub(10) } else { (inner * 2 / 5).clamp(36, 64) };
    let left_w = rows.iter().map(|r| 1 + r.name.width() + 1 + r.hint.width()).max().unwrap_or(0).min(cap);
    let body: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let left = truncate(&format!("{}{}{}{}", r.prefix, r.name, if r.hint.is_empty() { "" } else { " " }, r.hint), left_w);
            let pad = (left_w + 2).saturating_sub(left.width());
            let room = inner.saturating_sub(2 + left_w + 2);
            panel::row(i == sel, vec![Span::raw(format!("{left}{}", " ".repeat(pad))), Span::styled(truncate(&r.desc, room), theme::dim())])
        })
        .collect();
    let title = if rows[0].prefix == '@' { "Files" } else { "Commands" };
    let footer = theme::hints(&[("tab", "complete"), ("↑↓", "move"), ("esc", "hide")]);
    View::new(title, body, sel, footer).input_focus().rows(0, 8)
}

pub(crate) fn tab_line(app: &App) -> Line<'static> {
    Line::from(tab_groups(app).concat())
}

/// The tab strip's spans, one group per tab (so a click can be matched to its tab).
fn tab_groups(app: &App) -> Vec<Vec<Span<'static>>> {
    let mut groups = Vec::new();
    for (i, s) in app.sessions.iter().enumerate() {
        let mark = if !s.pending.is_empty() {
            crate::jobs::State::Blocked.glyph()
        } else if s.busy {
            crate::jobs::State::Working.glyph()
        } else if s.backend.is_none() && s.session_id.is_some() {
            "○"
        } else {
            " "
        };
        let title = if s.title.is_empty() { "new" } else { &s.title };
        // The tab wears its conversation's colour: a block when in front, the number and mark otherwise.
        let color = app.tab_color(i);
        let tint = theme::tint(color);
        let style = if i == app.active { theme::selected().patch(tint) } else { theme::dim() };
        let mark_style = match (i == app.active, mark) {
            (true, _) => style,
            (_, "◆") => theme::attention(),
            (_, "◐") => tint,
            _ => style,
        };
        let number = if i == app.active || color.is_none() { style } else { tint };
        let mut spans = Vec::new();
        spans.push(Span::styled(format!(" {} ", i + 1), number));
        if mark != " " {
            spans.push(Span::styled(format!("{mark} "), mark_style));
        }
        spans.push(Span::styled(format!("{title} "), style));
        spans.push(Span::raw(" "));
        groups.push(spans);
    }
    groups
}

fn transcript(f: &mut Frame, s: &mut Session, area: Rect, expanded: bool, cwd: &str) -> Vec<(u16, u16, usize)> {
    if s.items.is_empty() {
        f.render_widget(Paragraph::new(welcome(s, cwd, area.width as usize)), area);
        return Vec::new();
    }
    let width = area.width;
    let h = area.height as usize;
    // `keep: false` counts rows without holding the lines, so measuring a whole session never holds it all.
    let rows = |item: &mut Item, keep: bool| -> usize {
        match &item.cache {
            Some((w, n, _)) if *w == width => *n,
            _ => {
                let mut l = entry_lines(&item.entry, width as usize, expanded != item.flipped, cwd);
                let n = l.len();
                if !keep {
                    l = Vec::new();
                }
                item.cache = Some((width, n, l));
                n
            }
        }
    };
    // Following the newest output (the usual case) needs only the entries that fill the screen, so a
    // resize or a long session does not re-wrap everything above. Scrolled up, every entry's row count
    // is needed to know where the view is; counts are kept even where the lines were dropped.
    let following = s.scroll == 0 && s.reveal.is_none();
    let mut first = 0;
    let mut total = 0;
    if following {
        first = s.items.len();
        while first > 0 && total < h {
            first -= 1;
            total += rows(&mut s.items[first], true);
        }
        s.last_total = 0; // unknown, and not needed until the view is scrolled
    } else {
        for item in &mut s.items {
            total += rows(item, false);
        }
        // Scrolled up: keep the same lines in view while new output arrives below.
        if s.scroll > 0 && s.last_total > 0 && total > s.last_total {
            s.scroll += total - s.last_total;
        }
        s.last_total = total;
    }
    s.view_h = h;
    // `/find` asked for an entry: put its first line at the top of the view.
    if let Some(i) = s.reveal.take() {
        let above: usize = s.items[..i.min(s.items.len())].iter().map(|it| it.cache.as_ref().map_or(0, |c| c.1)).sum();
        s.scroll = total.saturating_sub(h + above.saturating_sub(1));
    }
    s.scroll = s.scroll.min(total.saturating_sub(h));
    let start = total.saturating_sub(h + s.scroll);
    let mut lines = Vec::with_capacity(h);
    let mut pos = 0;
    let mut calls = Vec::new();
    let mut shown = (first, first);
    for i in first..s.items.len() {
        let n = rows(&mut s.items[i], true);
        if pos + n > start {
            if s.items[i].cache.as_ref().is_some_and(|c| c.2.len() != n) {
                s.items[i].cache = None; // its lines were dropped: wrap it again
                rows(&mut s.items[i], true);
            }
            let item = &s.items[i];
            let l = &item.cache.as_ref().unwrap().2;
            let from = start.saturating_sub(pos);
            let at = lines.len();
            lines.extend(l[from..].iter().take(h - lines.len()).cloned());
            if matches!(item.entry, Entry::Tool { nested: false, .. }) {
                calls.push((area.y + at as u16, area.y + lines.len() as u16, i));
            }
            shown.1 = i;
            if at == 0 {
                shown.0 = i;
            }
            if lines.len() >= h {
                break;
            }
        }
        pos += n;
    }
    // Memory: rendered lines are kept only near the view (a long session held hundreds of MB of them).
    const KEEP: usize = 200;
    for (i, item) in s.items.iter_mut().enumerate() {
        if (i + KEEP < shown.0 || i > shown.1 + KEEP)
            && let Some(c) = item.cache.as_mut()
            && !c.2.is_empty()
        {
            c.2 = Vec::new();
        }
    }
    f.render_widget(Paragraph::new(lines), area);
    if s.scroll > 0 && h > 1 {
        let note = theme::keys(&format!(" ↓ {} lines below · ctrl+end ", s.scroll));
        let w = (note.chars().count() as u16).min(area.width);
        f.render_widget(Paragraph::new(Span::styled(note, theme::selected())), Rect::new(area.right() - w, area.bottom() - 1, w, 1));
    }
    calls
}

/// An empty tab: where you are, with what, and the keys that matter first. A blank screen says neither.
fn welcome(s: &Session, cwd: &str, width: usize) -> Vec<Line<'static>> {
    let dir = tilde(cwd);
    let inner = width.saturating_sub(6).clamp(10, 76);
    // Too long for the box: keep the end, where the project's own name is.
    let dir = if dir.width() > inner {
        let tail: String = dir.chars().rev().take(inner.saturating_sub(2)).collect::<Vec<_>>().into_iter().rev().collect();
        format!("…{}", tail.find('/').map_or(tail.as_str(), |i| &tail[i..]))
    } else {
        dir
    };
    let mut rows = vec![
        vec![Span::styled("✻ ", theme::working()), Span::styled("claude++", theme::bold()), Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), theme::dim())],
        vec![Span::styled(format!("  {dir}"), theme::dim())],
    ];
    if !s.model.is_empty() {
        rows.push(vec![Span::styled(format!("  {}{}", s.model, if s.effort.is_empty() { String::new() } else { format!(" · {}", s.effort) }), theme::dim())]);
    }
    let w = rows.iter().map(|r| r.iter().map(Span::width).sum::<usize>()).max().unwrap_or(0).min(inner);
    let frame = theme::working();
    let mut out = vec![Line::raw(""), Line::from(vec![Span::raw(" "), Span::styled(format!("╭{}╮", "─".repeat(w + 2)), frame)])];
    for r in rows {
        let pad = w.saturating_sub(r.iter().map(Span::width).sum::<usize>());
        let mut l = vec![Span::raw(" "), Span::styled("│ ", frame)];
        l.extend(r);
        l.push(Span::raw(" ".repeat(pad)));
        l.push(Span::styled(" │", frame));
        out.push(Line::from(l));
    }
    out.push(Line::from(vec![Span::raw(" "), Span::styled(format!("╰{}╯", "─".repeat(w + 2)), frame)]));
    out.push(Line::raw(""));
    let keys = theme::fit_hints(theme::hints(&[("enter", "send"), ("/help", "commands"), ("←", "earlier sessions"), ("ctrl+t", "new tab"), ("esc+esc", "rewind")]), width);
    out.push(keys);
    out
}

fn entry_lines(e: &Entry, width: usize, expanded: bool, cwd: &str) -> Vec<Line<'static>> {
    let mut out = vec![Line::raw("")];
    match e {
        Entry::User(t) => user_lines(&mut out, t, width, true),
        Entry::Rich(parts) => {
            let mut first = true;
            for part in parts {
                match part {
                    Part::Text(t) => user_lines(&mut out, t, width, first),
                    Part::Image(n, shown) => {
                        user_lines(&mut out, &crate::image::token(*n), width, first);
                        for mut l in shown.iter().flat_map(crate::image::placeholder_lines) {
                            l.spans.insert(0, Span::raw("  "));
                            out.push(l);
                        }
                    }
                }
                first = false;
            }
        }
        Entry::Assistant { text, streaming, .. } => {
            let text = if text.is_empty() && *streaming { "…" } else { text };
            markdown(&mut out, text, width);
        }
        Entry::Tool { name, input, result, nested, .. } => {
            tool_lines(&mut out, name, input, result, *nested, width, expanded, cwd);
            // Unfolded, the way back must be visible: only when folding would have hidden something.
            let mut folded = vec![Line::raw("")];
            if expanded && !*nested {
                tool_lines(&mut folded, name, input, result, false, width, false, cwd);
                if folded.len() < out.len() {
                    out.push(Line::styled(theme::keys("     ▴ fold (click or alt+o)"), theme::dim()));
                }
            }
        }
        Entry::Note(t) => block(&mut out, t, width, Span::raw("  "), theme::dim()),
        Entry::Error(t) => block(&mut out, t, width, Span::styled("✗ ", theme::danger()), theme::danger()),
        Entry::Image(s) => out.extend(crate::image::placeholder_lines(s)),
    }
    out
}

/// Your message, as Claude Code shows it: `❯` and the words on a band across the width.
fn user_lines(out: &mut Vec<Line<'static>>, text: &str, width: usize, first: bool) {
    let band = theme::user_band();
    for (i, l) in wrap(text, width.saturating_sub(3)).into_iter().enumerate() {
        let lead = if first && i == 0 { "❯ " } else { "  " };
        let pad = " ".repeat(width.saturating_sub(2 + l.width()));
        out.push(Line::from(vec![Span::styled(lead, band.patch(theme::accent()).add_modifier(Modifier::BOLD)), Span::styled(l + &pad, band)]));
    }
}

/// Wrap `text` under a first-line prefix, indenting continuation lines to match.
fn block(out: &mut Vec<Line<'static>>, text: &str, width: usize, prefix: Span<'static>, style: Style) {
    let indent = prefix.width();
    for (i, l) in wrap(text, width.saturating_sub(indent)).into_iter().enumerate() {
        let lead = if i == 0 { prefix.clone() } else { Span::raw(" ".repeat(indent)) };
        out.push(Line::from(vec![lead, Span::styled(l, style)]));
    }
}

/// Assistant text: rendered markdown under a `●` bullet.
fn markdown(out: &mut Vec<Line<'static>>, text: &str, width: usize) {
    for (i, mut l) in crate::markdown::render(text, width.saturating_sub(2)).into_iter().enumerate() {
        l.spans.insert(0, Span::raw(if i == 0 { "● " } else { "  " }));
        out.push(l);
    }
}

#[allow(clippy::too_many_arguments)]
fn tool_lines(
    out: &mut Vec<Line<'static>>,
    name: &str,
    input: &Value,
    result: &Option<(String, bool)>,
    nested: bool,
    width: usize,
    expanded: bool,
    cwd: &str,
) {
    // The model's thinking, as Claude Code shows it: one dim line, the text with ctrl+o or a click.
    if name == "Thinking" {
        let hint = if expanded { String::new() } else { theme::keys(" (ctrl+o or click to read)") };
        out.push(Line::styled(format!("✻ Thinking…{hint}"), theme::dim().add_modifier(Modifier::ITALIC)));
        if expanded {
            let text = result.as_ref().map_or("", |r| r.0.as_str());
            out.extend(wrap(text.trim(), width.saturating_sub(4)).into_iter().map(|l| Line::styled(format!("  {l}"), theme::dim().add_modifier(Modifier::ITALIC))));
        }
        return;
    }
    out.push(tool_header(name, input, result.is_none(), result.as_ref().is_some_and(|r| r.1), nested, width, cwd));
    if nested {
        out.remove(0); // subagent steps render as a compact list
        return;
    }
    if name == "Bash" {
        bash_command(out, input, width, expanded);
    }
    // A finished agent's steps, folded into its call: listed with ctrl+o, counted otherwise.
    let steps = input["_steps"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if expanded {
        for st in steps {
            out.push(tool_header(st["name"].as_str().unwrap_or("?"), &st["input"], false, false, true, width, cwd));
        }
    } else if !steps.is_empty() {
        let n = steps.len();
        out.push(Line::styled(theme::keys(&format!("    ↳ {n} step{} (click or ctrl+o to expand)", if n == 1 { "" } else { "s" })), theme::dim()));
    }
    if matches!(name, "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet") && result.as_ref().is_some_and(|r| !r.1) && !expanded {
        return; // the header says it, and the list above the input shows the state
    }
    if name == "TodoWrite" {
        for (i, t) in input["todos"].as_array().into_iter().flatten().enumerate() {
            let text = t["content"].as_str().unwrap_or_default();
            let (mark, style) = match t["status"].as_str() {
                Some("completed") => (theme::ON, theme::dim().add_modifier(Modifier::CROSSED_OUT)),
                Some("in_progress") => (theme::PART, theme::bold()),
                _ => (theme::OFF, Style::new()),
            };
            let lead = if i == 0 { "  ⎿  " } else { "     " };
            out.push(Line::from(vec![Span::raw(lead), Span::styled(truncate(&format!("{mark} {text}"), width.saturating_sub(5)), style)]));
        }
        if result.as_ref().is_none_or(|r| !r.1) {
            return; // the list is the result
        }
    }
    let limit = if expanded { usize::MAX } else { COLLAPSED_LINES * 3 };
    let rows = change_rows(name, input);
    let n = rows.iter().filter(|r| r.1 != ' ' && r.1 != '⋮').count();
    if let Some((_, false)) = result
        && n > 0
    {
        let count = |sign| rows.iter().filter(|r| r.1 == sign).count();
        let lines = |k: usize| format!("{k} line{}", if k == 1 { "" } else { "s" });
        let (added, removed) = (count('+'), count('-'));
        let said = match (name, input["_patch"].is_array(), added, removed) {
            ("Write", false, a, _) => format!("Wrote {}", lines(a)),
            (_, _, a, 0) => format!("Added {}", lines(a)),
            (_, _, 0, r) => format!("Removed {}", lines(r)),
            (_, _, a, r) => format!("Added {}, removed {}", lines(a), lines(r)),
        };
        out.push(Line::from(vec![Span::raw("  ⎿  "), Span::styled(truncate(&said, width.saturating_sub(5)), theme::dim())]));
    }
    let numw = rows.iter().filter_map(|r| r.0).max().map_or(0, |m| m.to_string().len());
    for (num, sign, text) in rows.iter().take(limit) {
        if *sign == '⋮' {
            out.push(Line::styled(format!("     {:>numw$}  ⋮", ""), theme::dim()));
            continue;
        }
        let gutter = format!("     {} ", num.map_or(" ".repeat(numw), |k| format!("{k:>numw$}")));
        let body = truncate(&format!("{sign} {text}"), width.saturating_sub(gutter.width()));
        let pad = " ".repeat(width.saturating_sub(gutter.width() + body.width()));
        out.push(match sign {
            '+' => Line::from(vec![Span::styled(gutter, theme::dim()), Span::styled(body + &pad, theme::added_line())]),
            '-' => Line::from(vec![Span::styled(gutter, theme::dim()), Span::styled(body + &pad, theme::removed_line())]),
            _ => Line::from(vec![Span::styled(gutter, theme::dim()), Span::raw(body)]),
        });
    }
    if rows.len() > limit {
        out.push(Line::styled(theme::keys(&format!("     … +{} lines (click or ctrl+o to expand)", rows.len() - limit)), theme::dim()));
    }
    let Some((text, err)) = result else { return };
    if n > 0 && !err {
        return; // the diff already says what happened
    }
    let style = if *err { theme::danger() } else { theme::dim() };
    if name == "Read" && !err && !expanded {
        // A file's contents are the model's business; Claude Code shows just the size too.
        let n = text.trim_end().lines().count();
        let what = if text.trim() == "[image]" { "Read image".into() } else { format!("Read {n} line{}", if n == 1 { "" } else { "s" }) };
        out.push(Line::from(vec![Span::raw("  ⎿  "), Span::styled(what, style)]));
        return;
    }
    let text = if text.trim().is_empty() { "(no output)" } else { text.trim_end() };
    if name == "AskUserQuestion" && !err {
        // `…answered: "Q"="A", "Q2"="B". You can now…` → one `· Q → A` line each.
        let answers: Vec<(&str, &str)> = text.split("\"=\"").collect::<Vec<_>>().windows(2).filter_map(|w| {
            let q = w[0].rsplit_once('"')?.1;
            let a = w[1].split_once('"')?.0;
            Some((q, a))
        }).collect();
        if !answers.is_empty() {
            for (i, (q, a)) in answers.into_iter().enumerate() {
                let lead = if i == 0 { "  ⎿  " } else { "     " };
                // The answer is the news: shorten the question, never the answer.
                let a = format!(" → {a}");
                let q = truncate(&format!("· {q}"), width.saturating_sub(5 + a.width()).max(6));
                out.push(Line::from(vec![Span::raw(lead), Span::styled(truncate(&(q + &a), width.saturating_sub(5)), theme::dim())]));
            }
            return;
        }
    }
    if name == "Bash" {
        bash_output(out, text, *err, width, expanded);
        return;
    }
    if matches!(name, "Agent" | "Task") && !err {
        // An agent's report is written in markdown, like Claude's own replies.
        let lines = crate::markdown::render(text, width.saturating_sub(5));
        let limit = if expanded { usize::MAX } else { COLLAPSED_LINES * 3 };
        let n = lines.len();
        for (i, mut l) in lines.into_iter().take(limit).enumerate() {
            l.spans.insert(0, Span::raw(if i == 0 { "  ⎿  " } else { "     " }));
            out.push(l);
        }
        if n > limit {
            out.push(Line::styled(theme::keys(&format!("     … +{} lines (click or ctrl+o to expand)", n - limit)), theme::dim()));
        }
        return;
    }
    let (lines, hidden) = if expanded { (wrap(text, width.saturating_sub(5)), 0) } else { fold(text, width.saturating_sub(5), false) };
    for (i, l) in lines.into_iter().enumerate() {
        let lead = if i == 0 { "  ⎿  " } else { "     " };
        out.push(Line::from(vec![Span::raw(lead), Span::styled(l, style)]));
    }
    if hidden > 0 {
        out.push(Line::styled(theme::keys(&format!("     … +{hidden} lines (click or ctrl+o to expand)")), theme::dim()));
    }
}

/// A Bash call's command when the header cannot hold it (several lines, or longer than the row), and what
/// it is for: shown unfolded, hinted at folded.
fn bash_command(out: &mut Vec<Line<'static>>, input: &Value, width: usize, expanded: bool) {
    let cmd = input["command"].as_str().unwrap_or_default().trim_end();
    let long = cmd.contains('\n') || cmd.width() + 4 > width;
    if !expanded {
        if long {
            out.push(Line::styled(theme::keys("    … whole command (click or ctrl+o to expand)"), theme::dim()));
        }
        return;
    }
    if let Some(d) = input["description"].as_str().filter(|d| !d.is_empty()) {
        out.push(Line::styled(truncate(&format!("    {d}"), width), theme::dim().add_modifier(Modifier::ITALIC)));
    }
    if long {
        for l in wrap(cmd, width.saturating_sub(6)) {
            out.push(Line::from(vec![Span::styled("    │ ", theme::dim()), Span::styled(l, theme::tool_hue("Bash"))]));
        }
    }
}

/// A command's output under `⎿`. A passing command shows its first lines, a failing one its last: that is
/// where the error is.
fn bash_output(out: &mut Vec<Line<'static>>, text: &str, err: bool, width: usize, expanded: bool) {
    let (lines, hidden) = if expanded { (wrap(text, width.saturating_sub(5)), 0) } else { fold(text, width.saturating_sub(5), err) };
    let start = out.len();
    let lead = |out: &Vec<Line>| if out.len() == start { "  ⎿  " } else { "     " };
    if err && hidden > 0 {
        out.push(Line::from(vec![Span::raw(lead(out)), Span::styled(theme::keys(&format!("… {hidden} earlier lines (click or ctrl+o to expand)")), theme::dim())]));
    }
    let style = if err { theme::danger() } else { theme::dim() };
    for l in lines {
        out.push(Line::from(vec![Span::raw(lead(out)), Span::styled(l, style)]));
    }
    if !err && hidden > 0 {
        out.push(Line::styled(theme::keys(&format!("     … +{hidden} lines (click or ctrl+o to expand)")), theme::dim()));
    }
}

/// Folded output: its first `COLLAPSED_LINES` rows (the last, for an error) and how many lines that leaves
/// out. Wraps only what is shown, so folding a huge output costs what folding a short one does.
fn fold(text: &str, width: usize, tail: bool) -> (Vec<String>, usize) {
    let total = text.split('\n').count();
    let mut rows = Vec::new();
    let mut used = 0;
    let src: Box<dyn Iterator<Item = &str>> = if tail { Box::new(text.rsplit('\n')) } else { Box::new(text.split('\n')) };
    for line in src {
        let mut w = wrap(line, width);
        if rows.len() + w.len() > COLLAPSED_LINES && used > 0 {
            break;
        }
        if tail {
            w.reverse();
        }
        rows.extend(w);
        used += 1;
        if rows.len() >= COLLAPSED_LINES {
            break;
        }
    }
    let cut = rows.len() > COLLAPSED_LINES; // one line longer than the fold: it counts as left out
    rows.truncate(COLLAPSED_LINES);
    if tail {
        rows.reverse();
    }
    (rows, total - used + cut as usize)
}

/// The lines an Edit or Write changes, as (line number, sign, text): `+` added, `-` removed, ` ` context,
/// `⋮` a gap between hunks. From the hunks Claude Code reported once the call ran (with line numbers), else
/// worked out from the call itself (a pending call, an old transcript).
fn change_rows(name: &str, input: &Value) -> Vec<(Option<usize>, char, String)> {
    let mut rows = Vec::new();
    if let Some(hunks) = input["_patch"].as_array() {
        for (i, h) in hunks.iter().enumerate() {
            if i > 0 {
                rows.push((None, '⋮', String::new()));
            }
            let (mut old, mut new) = (h["oldStart"].as_u64().unwrap_or(1) as usize, h["newStart"].as_u64().unwrap_or(1) as usize);
            for l in h["lines"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let (sign, text) = l.split_at(l.chars().next().map_or(0, char::len_utf8));
                match sign {
                    "+" => (rows.push((Some(new), '+', text.into())), new += 1),
                    "-" => (rows.push((Some(old), '-', text.into())), old += 1),
                    " " => (rows.push((Some(new), ' ', text.into())), (old, new) = (old + 1, new + 1)),
                    _ => ((), ()), // "\\ No newline at end of file"
                };
            }
        }
        return rows;
    }
    match name {
        "Edit" => {
            let edits: Vec<&Value> = input["edits"].as_array().map_or_else(|| vec![input], |a| a.iter().collect());
            for (i, e) in edits.into_iter().enumerate() {
                if i > 0 {
                    rows.push((None, '⋮', String::new()));
                }
                let (a, b) = (e["old_string"].as_str().unwrap_or_default(), e["new_string"].as_str().unwrap_or_default());
                rows.extend(line_diff(a, b).into_iter().map(|(sign, t)| (None, sign, t.to_string())));
            }
        }
        "Write" => rows.extend(input["content"].as_str().unwrap_or_default().lines().enumerate().map(|(i, l)| (Some(i + 1), '+', l.to_string()))),
        _ => {}
    }
    rows
}

/// A line diff of `a` into `b` (longest common subsequence). Past a size where that gets slow, all of `a`
/// removed then all of `b` added.
fn line_diff<'a>(a: &'a str, b: &'a str) -> Vec<(char, &'a str)> {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    if a.len() * b.len() > 250_000 {
        return a.iter().map(|l| ('-', *l)).chain(b.iter().map(|l| ('+', *l))).collect();
    }
    // lcs[i][j]: common lines of a[i..] and b[j..]
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push((' ', a[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(('-', a[i]));
            i += 1;
        } else {
            out.push(('+', b[j]));
            j += 1;
        }
    }
    out
}

/// The `● Name(summary)` line of a tool call; `nested` makes it a subagent's `↳` step.
fn tool_header(name: &str, input: &Value, running: bool, failed: bool, nested: bool, width: usize, cwd: &str) -> Line<'static> {
    // still running: receded; finished: green or red. (Yellow is reserved for "waiting on you".)
    let role = if running {
        theme::dim()
    } else if failed {
        theme::danger()
    } else {
        theme::ok()
    };
    let pad = if nested { "    " } else { "" };
    let summary = tool_summary(name, input, cwd);
    let (lead, text) = match tool_icon(name) {
        Some(icon) => (format!("{pad}{}{icon} ", if nested { "↳ " } else { "" }), summary),
        None => (format!("{pad}{}", if nested { "↳ " } else { "● " }), format!("{}({summary})", tool_name(name))),
    };
    let text = truncate(&text, width.saturating_sub(lead.width()));
    let icon_style = if tool_icon(name).is_some() { theme::tool_hue(name) } else { role }.add_modifier(Modifier::BOLD);
    Line::from(vec![Span::styled(lead, icon_style), Span::styled(text, role.add_modifier(Modifier::BOLD))])
}

/// One glyph per kind of tool, so a run of tool calls reads by shape. Unknown (MCP...) tools keep their name.
pub fn tool_icon(name: &str) -> Option<&'static str> {
    Some(match name {
        "Bash" => "$",
        "Read" => "≡",
        "Edit" | "NotebookEdit" => "±",
        "Write" => "+",
        "Glob" | "Grep" => "⌕",
        "WebFetch" | "WebSearch" => "◎",
        "Task" | "Agent" => "◈",
        "Skill" => "✦",
        "AskUserQuestion" => "?",
        "TodoWrite" => theme::ON,
        _ => return None,
    })
}

pub fn tool_summary(name: &str, input: &Value, cwd: &str) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let path = |k: &str| {
        let p = s(k);
        p.strip_prefix(cwd).map(|r| r.trim_start_matches('/').to_string()).unwrap_or(p)
    };
    let v = match name {
        "Bash" => s("command"),
        "Read" | "Write" | "Edit" | "NotebookEdit" => path("file_path"),
        "Glob" | "Grep" => s("pattern"),
        "WebFetch" => s("url"),
        "WebSearch" => s("query"),
        "Task" | "Agent" => s("description"),
        "Skill" => s("skill"),
        "AskUserQuestion" => input["questions"][0]["question"].as_str().unwrap_or_default().to_string(),
        "ExitPlanMode" => "plan ready for approval".into(),
        "TaskCreate" => s("subject"),
        "TaskUpdate" => {
            let id = input["taskId"].as_str().map(String::from).unwrap_or_else(|| input["taskId"].to_string());
            let what = input["status"].as_str().or(input["subject"].as_str()).unwrap_or("updated").replace('_', " ");
            format!("#{} → {what}", id.trim_start_matches('#'))
        }
        "TodoWrite" => {
            let todos = input["todos"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            format!("{}/{} done", todos.iter().filter(|t| t["status"] == "completed").count(), todos.len())
        }
        _ => String::new(),
    };
    let v = if v.is_empty() { brief(input) } else { v };
    truncate(&v.replace('\n', " ⏎ "), 200)
}

/// Any other tool's input, readably: the first text field as is, the rest as `key: value`.
fn brief(input: &Value) -> String {
    let Some(o) = input.as_object() else { return input.to_string() };
    let mut parts = o.iter().filter(|(k, _)| !k.starts_with('_')).enumerate().map(|(i, (k, v))| match v {
        Value::String(t) if i == 0 => t.clone(),
        Value::String(t) => format!("{k}: {t}"),
        v => format!("{k}: {v}"),
    });
    let first = parts.next().unwrap_or_default();
    parts.fold(first, |acc, p| format!("{acc}, {p}"))
}

/// A tool's name as shown: an MCP tool's `mcp__server__tool` reads `server · tool`.
fn tool_name(name: &str) -> String {
    match name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
        Some((server, tool)) => format!("{server} · {tool}"),
        None => name.to_string(),
    }
}

/// What the panel for the prompt at the front of the queue shows.
fn prompt_view(p: &Pending, width: usize, cwd: &str) -> View {
    if p.is_question() {
        return p.ui.question_view(p.questions(), width);
    }
    let always = crate::prompt::Ui::always_label(&p.suggestions);
    if p.tool == "Bash" {
        // The command on its own lines, highlighted, with what Claude says it is for: as Claude Code shows it.
        let cmd = p.input["command"].as_str().unwrap_or_default();
        let fence = if cmd.contains("```") { "~~~" } else { "```" };
        let mut shown = format!("{fence}bash\n{cmd}\n{fence}\n");
        if let Some(d) = p.input["description"].as_str().filter(|d| !d.is_empty()) {
            shown += &format!("\n*{d}*\n");
        }
        return p.ui.permission_view("Run this command?", Some(&shown), always.as_deref(), width);
    }
    let headline = if p.tool == "ExitPlanMode" {
        "Claude's plan is ready. Start on it?".to_string()
    } else {
        format!("Allow {}({})?", tool_name(&p.tool), tool_summary(&p.tool, &p.input, cwd))
    };
    p.ui.permission_view(&headline, p.input["plan"].as_str(), always.as_deref(), width)
}

/// `✻ Thinking… (12s · ↓ 340 tokens · esc to interrupt)`, as Claude Code shows a turn in progress.
fn spinner_line(s: &Session, width: usize) -> Line<'static> {
    let el = s.busy_since.elapsed();
    let frame = SPINNER[(el.as_millis() / 120) as usize % SPINNER.len()];
    let what = if s.activity.is_empty() { "working" } else { &s.activity };
    let mut what: Vec<char> = what.chars().collect();
    what[0] = what[0].to_ascii_uppercase();
    let mut detail = crate::session::duration(el);
    if s.turn_chars >= 4 {
        detail += &format!(" · ↓ {} tokens", crate::usage::tokens((s.turn_chars / 4) as u64));
    }
    let line = Line::from(vec![
        Span::styled(format!("{frame} "), theme::working()),
        Span::styled(format!("{}…", what.into_iter().collect::<String>()), theme::working().add_modifier(Modifier::BOLD)),
        Span::styled(theme::keys(&format!(" ({detail} · esc to interrupt{})", if s.shells.iter().any(|x| x.status.is_none() && !x.background) { " · ctrl+b to background" } else { "" })), theme::dim()),
    ]);
    if line.width() > width { Line::from(line.spans[..2].to_vec()) } else { line }
}

/// One part of claude++'s own status line (`segments::ALL`), or nothing when it has nothing to say yet.
fn segment(app: &App, s: &Session, id: &str, ext: bool) -> Option<(String, Style)> {
    Some(match id {
        "profile" if !s.profile.is_empty() && s.profile != "full" => (s.profile.clone(), theme::mode("acceptEdits")),
        "model" if !s.model.is_empty() && !ext => {
            let base = if s.effort.is_empty() { s.model.clone() } else { format!("{} {}", s.model, s.effort) };
            (if s.fast_mode { format!("{base} ↯") } else { base }, theme::accent())
        }
        "ctx" if s.ctx_tokens > 0 && !ext => {
            // the context filling up is the one number worth a warning colour
            let hot = s.ctx_tokens * 100 >= s.ctx_window.max(200_000) * 85;
            // as a share of the window once claude has reported its size, like Claude Code's own line
            let pct = (s.ctx_tokens * 100).checked_div(s.ctx_window).map_or(String::new(), |p| format!(" {p}%"));
            (format!("ctx {}{pct}", crate::usage::tokens(s.ctx_tokens)), if hot { theme::danger() } else { theme::dim() })
        }
        "hit" if s.prompt_total > 0 => (format!("hit {:.0}%", 100.0 * s.cache_read as f64 / s.prompt_total as f64), theme::ok()),
        "cache" => {
            let left = s.cache_left(app.cache_ttl)?;
            if left.is_zero() { ("◷ cold".into(), theme::danger()) } else { (format!("◷ {}m", left.as_secs().div_ceil(60)), theme::dim()) }
        }
        "cost" if s.cost > 0.0 && !ext => (format!("${:.3}", s.cost), theme::ok()),
        "limits" if !s.rate.is_empty() && !ext => (s.rate.clone(), theme::dim()),
        "mem" => {
            let backends: u64 = app.sessions.iter().map(|s| s.mem_kb).sum();
            (format!("mem {}M+{}M", backends / 1024, app.ui_mem_kb / 1024), theme::dim())
        }
        "dir" => (tilde(s.cwd.as_deref().unwrap_or(&app.cwd)), theme::dim()),
        "branch" => (format!("⎇ {}", crate::segments::branch(s.cwd.as_deref().unwrap_or(&app.cwd))?), theme::dim()),
        "clock" => (crate::queue::clock(std::time::SystemTime::now()), theme::dim()),
        "version" if !s.claude_version().is_empty() => (format!("cc {}", s.claude_version()), theme::dim()),
        _ => return None,
    })
}

fn status_line(app: &App, f_height: u16) -> Line<'static> {
    let s = &app.sessions[app.active];
    // With the user's status line showing, ours keeps only what theirs cannot know: no model, context,
    // cost or limits (their script has those), but the prompt-cache clock, memory and mode.
    let ext = app.statusline_on && !s.status_lines.is_empty() && s.pending.is_empty() && f_height >= 14;
    let mut spans = Vec::new();
    let mode_style = theme::mode(&s.mode);
    // The permission mode is always shown, in words: it decides whether Claude asks before acting.
    let mode = match s.mode.as_str() {
        "" | "default" => "◇ manual",
        "acceptEdits" => "⏵⏵ accept edits",
        "plan" => "◈ plan",
        "auto" => "✦ auto",
        "bypassPermissions" => "! bypass",
        "dontAsk" => "✗ don't ask",
        m => m,
    };
    spans.push(Span::styled(mode.to_string(), mode_style.add_modifier(Modifier::BOLD)));
    spans.push(Span::styled(format!(" {}  ", theme::key_glyph("shift+tab")), theme::dim()));
    let parts: Vec<(String, Style)> = app.line_items.iter().filter_map(|id| segment(app, s, id, ext)).collect();
    if let Some(msg) = &app.flash {
        spans.insert(0, Span::styled(format!("{msg}  "), theme::accent()));
    }
    for (i, (text, style)) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// `path` with the home folder shown as `~`.
pub fn tilde(path: &str) -> String {
    let rest = std::env::home_dir().and_then(|h| std::path::Path::new(path).strip_prefix(h).ok().map(|r| r.display().to_string()));
    match rest {
        Some(r) if r.is_empty() => "~".into(),
        Some(r) => format!("~/{r}"),
        None => path.to_string(),
    }
}

pub fn truncate(s: &str, width: usize) -> String {
    let s = &s.replace('\t', "    "); // a tab has no width in the terminal buffer
    if s.width() <= width {
        return s.to_string();
    }
    let mut w = 0;
    let mut out: String = s
        .chars()
        .take_while(|c| {
            w += c.width().unwrap_or(0);
            w < width
        })
        .collect();
    out.push('…');
    out
}

/// Greedy word wrap by display width; words wider than the line are hard-broken
/// (which also handles CJK text without spaces).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.replace('\t', "    ").split('\n') {
        let mut line = String::new();
        let mut w = 0;
        for tok in para.split_inclusive(' ') {
            let word_w = tok.trim_end_matches(' ').width();
            if w > 0 && w + word_w > width {
                out.push(std::mem::take(&mut line).trim_end().to_string());
                w = 0;
            }
            if word_w > width {
                for ch in tok.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if w + cw > width {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                    line.push(ch);
                    w += cw;
                }
            } else {
                line.push_str(tok);
                w += tok.width();
            }
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// Hard-wrap the input by width (no word wrap, so cursor math stays exact).
/// Returns the lines and the cursor's (col, row).
/// The input box's lines. A line that is just an `[Image #n]` token gets that picture's preview rows right
/// under it, so the picture sits among the text where it was pasted. Returns the rows and the row the cursor is on.
fn input_rows(lines: &[String], cursor_line: usize, atts: &[crate::session::Attachment], text: &[char], sel: Option<(usize, usize)>) -> (Vec<Line<'static>>, usize) {
    let mut rows = Vec::with_capacity(lines.len());
    let mut cursor_row = 0;
    let mut start = 0; // the text position this row begins at
    // Shell commands and slash commands are not markdown: a `*` there is a glob.
    let plain = matches!(text.iter().find(|c| !c.is_whitespace()), Some('!' | '/'));
    let styles = if plain { vec![] } else { crate::markdown::source_styles(&text.iter().collect::<String>()) };
    for (i, l) in lines.iter().enumerate() {
        if i == cursor_line {
            cursor_row = rows.len();
        }
        let lead = if i == 0 { Span::styled("❯ ", theme::accent().add_modifier(Modifier::BOLD)) } else { Span::raw("  ") };
        let n = l.chars().count();
        // Markdown styling with the markers kept; a vim selection is drawn in reverse video over it.
        let mut body: Vec<Span<'static>> = Vec::new();
        for (j, c) in l.chars().enumerate() {
            let at = start + j;
            let st = if sel.is_some_and(|(a, b)| (a..b).contains(&at)) { theme::selected() } else { styles.get(at).copied().unwrap_or_default() };
            match body.last_mut() {
                Some(sp) if sp.style == st => sp.content.to_mut().push(c),
                _ => body.push(Span::styled(c.to_string(), st)),
            }
        }
        start += n + usize::from(text.get(start + n) == Some(&'\n'));
        rows.push(Line::from([vec![lead], body].concat()));
        let shown = crate::image::token_n(l.trim()).and_then(|n| atts.iter().find(|a| a.n == n)).and_then(|a| a.preview);
        for mut p in shown.iter().flat_map(crate::image::placeholder_lines) {
            p.spans.insert(0, Span::raw("  "));
            rows.push(p);
        }
    }
    (rows, cursor_row)
}

pub(crate) fn input_layout(text: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let mut lines = vec![String::new()];
    let mut w = 0;
    let mut pos = (0, 0);
    for (i, ch) in text.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if ch != '\n' && w + cw > width {
            lines.push(String::new());
            w = 0;
        }
        if i == cursor {
            pos = (w, lines.len() - 1);
        }
        if ch == '\n' {
            lines.push(String::new());
            w = 0;
            continue;
        }
        lines.last_mut().unwrap().push(ch);
        w += cw;
    }
    if cursor >= text.chars().count() {
        if w >= width {
            lines.push(String::new());
            w = 0;
        }
        pos = (w, lines.len() - 1);
    }
    (lines, pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_replaces_the_home_folder_by_whole_path_components_only() {
        let home = std::env::home_dir().expect("tests run with a home folder").display().to_string();
        assert_eq!(tilde(&home), "~");
        assert_eq!(tilde(&format!("{home}/src/x")), "~/src/x");
        assert_eq!(tilde(&format!("{home}xyz/src")), format!("{home}xyz/src"), "a sibling that only shares the prefix is not under home");
        assert_eq!(tilde("/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn wrap_words_and_wide_chars() {
        assert_eq!(wrap("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap("a\n\nb", 5), vec!["a", "", "b"]);
        assert_eq!(wrap("1\tfn", 20), vec!["1    fn"], "tabs keep their space");
        assert_eq!(wrap("안녕하세요", 4), vec!["안녕", "하세", "요"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("    indented code", 40), vec!["    indented code"]);
    }

    #[test]
    fn input_cursor_tracks_wraps_and_newlines() {
        assert_eq!(input_layout("abc", 3, 10).1, (3, 0));
        assert_eq!(input_layout("ab\ncd", 3, 10).1, (0, 1));
        assert_eq!(input_layout("abcd", 4, 4), (vec!["abcd".into(), "".into()], (0, 1)));
        assert_eq!(input_layout("abcde", 4, 4).1, (0, 1), "a cursor on the wrapped char is on the next row");
        assert_eq!(input_layout("한글", 1, 10).1, (2, 0));
    }

    #[test]
    fn edit_tool_shows_diff_not_result() {
        let input = serde_json::json!({"file_path": "/p/a.rs", "old_string": "x", "new_string": "y\nz"});
        let mut out = Vec::new();
        tool_lines(&mut out, "Edit", &input, &Some(("ok".into(), false)), false, 80, false, "/p");
        let text: Vec<String> = out.iter().map(|l| l.to_string().trim_end().to_string()).collect();
        assert_eq!(text, vec!["± a.rs", "  ⎿  Added 2 lines, removed 1 line", "      - x", "      + y", "      + z"]);
        // Once it ran, Claude Code's own hunks: line numbers, context, no "no newline" marker; changes are banded.
        let input = serde_json::json!({"file_path": "/p/a.rs", "_patch": [
            {"oldStart": 9, "newStart": 9, "lines": [" a", "-b", "+c", "\\ No newline at end of file"]},
            {"oldStart": 20, "newStart": 20, "lines": ["+d"]}]});
        let mut out = Vec::new();
        tool_lines(&mut out, "Edit", &input, &Some(("ok".into(), false)), false, 30, false, "/p");
        let text: Vec<String> = out.iter().map(|l| l.to_string().trim_end().to_string()).collect();
        assert_eq!(text, vec!["± a.rs", "  ⎿  Added 2 lines, removed 1…", "      9   a", "     10 - b", "     10 + c", "         ⋮", "     20 + d"]);
        assert!(out[4].spans[1].style.bg.is_some() && out[4].width() == 30, "an added line is a band across the width");
        assert!(out.iter().all(|l| l.width() <= 30));
    }

    #[test]
    fn other_tools_read_as_words_and_todos_as_a_checklist() {
        let cwd = "/p";
        assert_eq!(tool_summary("ToolSearch", &serde_json::json!({"query": "select:X", "max_results": 1}), cwd), "select:X, max_results: 1");
        let mut out = Vec::new();
        let input = serde_json::json!({"a": 1});
        tool_lines(&mut out, "mcp__codegraph__explore", &input, &None, false, 80, false, cwd);
        assert_eq!(out[0].to_string(), "● codegraph · explore(a: 1)");
        let todos = serde_json::json!({"todos": [
            {"content": "plan", "status": "completed"}, {"content": "edit", "status": "in_progress"}, {"content": "test", "status": "pending"}]});
        let mut out = Vec::new();
        tool_lines(&mut out, "TodoWrite", &todos, &Some(("Todos have been modified".into(), false)), false, 80, false, cwd);
        let text: Vec<String> = out.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, ["☑ 1/3 done", "  ⎿  ☑ plan", "     ▣ edit", "     ☐ test"]);
        let mut out = Vec::new();
        tool_lines(&mut out, "Agent", &serde_json::json!({"description": "look"}), &Some(("Found **two** files:\n- `a`".into(), false)), false, 80, false, cwd);
        let text: Vec<String> = out.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, ["◈ look", "  ⎿  Found two files:", "     • a"], "an agent's report is markdown");
        let mut out = Vec::new();
        let said = r#"User has answered your questions: "Which file?"="calc.py", "Why?"="short". You can now continue with the user's answers in mind."#;
        tool_lines(&mut out, "AskUserQuestion", &serde_json::json!({}), &Some((said.into(), false)), false, 80, false, cwd);
        let text: Vec<String> = out.iter().skip(1).map(|l| l.to_string()).collect();
        assert_eq!(text, ["  ⎿  · Which file? → calc.py", "     · Why? → short"]);
        let mut out = Vec::new();
        tool_lines(&mut out, "AskUserQuestion", &serde_json::json!({}), &Some((said.into(), false)), false, 24, false, cwd);
        assert_eq!(out[1].to_string(), "  ⎿  · Which … → calc.py");
    }

    #[test]
    fn a_line_diff_keeps_unchanged_lines_as_context() {
        assert_eq!(line_diff("a\nb\nc", "a\nB\nc\nd"), vec![(' ', "a"), ('-', "b"), ('+', "B"), (' ', "c"), ('+', "d")]);
    }
}

/// Up to 5 rows of the task list while any task is open: the one in progress bold, done ones struck out.
fn task_strip(tasks: &[(String, String, String)], width: usize) -> Vec<Line<'static>> {
    if tasks.iter().all(|t| t.2 == "completed") {
        return vec![];
    }
    let shown = 5;
    // Keep the work in progress in view: start the window at the first unfinished task, done ones before it fill up.
    let first_open = tasks.iter().position(|t| t.2 != "completed").unwrap_or(0);
    let start = first_open.saturating_sub(1).min(tasks.len().saturating_sub(shown));
    let mut out: Vec<Line<'static>> = tasks[start..]
        .iter()
        .take(shown)
        .enumerate()
        .map(|(i, (_, subject, status))| {
            let (mark, style) = match status.as_str() {
                "completed" => (theme::ON, theme::dim().add_modifier(Modifier::CROSSED_OUT)),
                "in_progress" => (theme::PART, theme::bold()),
                _ => (theme::OFF, Style::new()),
            };
            let lead = if i == 0 { "  ⎿  " } else { "     " };
            Line::from(vec![Span::styled(lead, theme::dim()), Span::styled(truncate(&format!("{mark} {subject}"), width.saturating_sub(6)), style)])
        })
        .collect();
    let hidden = tasks.len() - out.len();
    if hidden > 0 {
        let done = tasks.iter().filter(|t| t.2 == "completed").count();
        out.push(Line::styled(format!("     … +{hidden} more ({done}/{} done)", tasks.len()), theme::dim()));
    }
    out
}
#[cfg(test)]
#[test]
fn folding_wraps_only_what_shows_and_counts_the_rest() {
    let text: String = (1..=400).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
    let (rows, hidden) = fold(&text, 80, false);
    assert_eq!((rows.len(), rows[0].as_str(), hidden), (COLLAPSED_LINES, "line 1", 400 - COLLAPSED_LINES));
    let (rows, hidden) = fold(&text, 80, true);
    assert_eq!((rows.last().unwrap().as_str(), hidden), ("line 400", 400 - COLLAPSED_LINES));
    assert_eq!(fold("short\nout", 80, false), (vec!["short".to_string(), "out".to_string()], 0));
    let (rows, hidden) = fold(&"x".repeat(1000), 10, false);
    assert_eq!((rows.len(), hidden), (COLLAPSED_LINES, 1));
    let s = std::time::Instant::now();
    let big: String = (0..100_000).map(|i| format!("line {i}\n")).collect();
    fold(&big, 80, false);
    assert!(s.elapsed() < std::time::Duration::from_millis(250), "{:?}", s.elapsed());
}
