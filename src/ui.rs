//! Rendering. Each transcript entry is wrapped once per width and cached, and only
//! the visible window of lines is handed to ratatui, so redraw cost stays flat as
//! the conversation grows.

use crate::panel::{self, View};
use crate::{App, instructions, optimize, permissions, settings, supervisor, tabcfg, theme, usage};
use crate::session::{Entry, Item, Part, Pending, Session, Spot};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const COLLAPSED_LINES: usize = 4;

/// The docked list needs a screen at least this wide, and takes this many columns.
const DOCK_MIN_SCREEN: u16 = 100;
const DOCK_W: u16 = 34;

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

/// `/term`: the terminal takes the whole screen (a hint row stays at the bottom); the conversation keeps updating behind it.
fn draw_term_full(f: &mut Frame, app: &mut App) {
    let waiting = !app.sessions[app.active].pending.is_empty();
    let [body, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let Some(t) = app.term_view.as_ref() else { return };
    t.resize(body.height, body.width);
    if let Some((x, y)) = t.render(f.buffer_mut(), body) {
        f.set_cursor_position(Position::new(x, y));
    }
    let who = if t.by_agent { "opened by Claude" } else if t.shared.load(std::sync::atomic::Ordering::Relaxed) { "shared with Claude" } else { "private" };
    let state = t.exit().map_or(String::new(), |s| format!(" · {s}"));
    let mut spans = vec![Span::styled(format!(" {} · {} · {who}{state} · ctrl+\\ back (it keeps running)", t.name, truncate(&t.command, 30)), theme::dim())];
    if waiting {
        spans.push(Span::styled(" · ◆ Claude is waiting for you", theme::attention()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), foot);
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.sync_selection(); // every list rests on the same conversation
    app.panes = vec![f.area()];
    app.input_rect = Rect::default(); // set again where the message box is drawn
    draw_ui(f, app);
    select(f, app); // after everything is drawn, so the highlight and the copy cover panels too
}

fn draw_ui(f: &mut Frame, app: &mut App) {
    if app.term_view.is_some() {
        return draw_term_full(f, app);
    }
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
        panel::render_full(f, area, v);
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
        }
        app.panes = vec![dock, rest]; // a drag selects within the pane it started in
        area = rest;
    }
    let input_w = area.width.saturating_sub(2).max(1) as usize;
    app.input_w = input_w;
    let (input_lines, cursor) = input_layout(&app.input, app.cursor, input_w);
    // Whatever panel is open (a prompt, a picker, config, suggestions...) is built as a `View`, sized and
    // drawn by the one shared panel renderer, at the bottom of the screen above the input.
    let view = surface(app, area.width as usize);
    let panel_h = view.as_ref().map_or(0, |v| panel::height(v, area.height));
    // A panel stands in for the message box (the suggestions are the exception: they are about it).
    let hide_input = panel_h > 0 && view.as_ref().is_some_and(|v| !v.input_focus);
    // The terminal cursor belongs to whatever has the keyboard: not the input box while a panel or the side list does.
    let input_keeps_focus = view.as_ref().is_none_or(|v| v.input_focus) && !app.dock_focus;
    let s = &app.sessions[app.active];
    let chars: Vec<char> = app.input.chars().collect();
    let sel = app.vim.on.then(|| app.vim.highlight(&chars, app.cursor)).flatten().or(app.box_selection());
    let (input_rows, cursor_row) = input_rows(&input_lines, cursor.1, &s.attachments, &chars, sel);
    // The user's own status line (if configured and it has produced output) sits under the input.
    // Like Claude Code, it steps aside while a permission prompt or question is up, and on tiny terminals.
    let ext_h = if app.statusline_on && s.pending.is_empty() && !hide_input && area.height >= 14 {
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
        Constraint::Length(if hide_input { 0 } else { (if app.pane.is_some() { pane_rows(area.height) } else { input_rows.len().min((area.height / 2).max(8) as usize) as u16 }) + 2 }),
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
    }
    if spin_h > 0 {
        f.render_widget(Paragraph::new(spinner_line(s, spin_area.width as usize)), spin_area);
    }

    f.render_widget(Paragraph::new(tab_line(app)), tabs);
    let expanded = app.expanded;
    let cwd = app.cwd.clone();
    // Room at the sides, so text does not run into the panel's edge or the screen's. Bands (your message,
    // diff lines) ignore it and run to the edge.
    let side = if body.width >= 70 { 2 } else { 0 };
    let peek = if app.dock_peek && app.dock_focus { app.dock.as_mut().map(|d| d.peek_lines(body.width.saturating_sub(2 * side) as usize)) } else { None };
    match peek {
        Some(lines) => {
            let skip = lines.len().saturating_sub(body.height as usize);
            f.render_widget(Paragraph::new(lines).scroll((skip as u16, 0)), body.inner(ratatui::layout::Margin::new(side, 0)));
        }
        None => {
            transcript(f, &mut app.sessions[app.active], body, side, expanded, &cwd);
        }
    }
    app.flush_selection(); // the selection was read while the conversation was laid out; its "copied" flash is drawn below
    if let Some(view) = view.filter(|_| panel_h > 0) {
        panel::render(f, panel_area, view);
    }

    if hide_input {
        f.render_widget(Paragraph::new(status_line(app, area.height)), status);
        return;
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
    app.input_rect = Rect::new(input_area.x + 2, input_area.y + 1, input_area.width.saturating_sub(2), visible as u16);
    app.input_skip = skip;
    // A panel that owns the keyboard also owns the cursor: leave it hidden.
    if input_keeps_focus {
        f.set_cursor_position(Position::new(input_area.x + 2 + cursor.0 as u16, input_area.y + 1 + cursor_row.saturating_sub(skip) as u16));
    }
    f.render_widget(Paragraph::new(status_line(app, area.height)), status);
}

/// The character index under (row, column) of the message box's wrapped text, as `input_layout` lays it out: a column
/// past the end of a row is that row's end.
pub(crate) fn input_index(text: &str, width: usize, row: usize, col: usize) -> usize {
    let (mut r, mut w) = (0, 0);
    for (i, ch) in text.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if ch != '\n' && w + cw > width {
            (r, w) = (r + 1, 0);
        }
        if r > row || (r == row && (ch == '\n' || col < w + cw)) {
            return i;
        }
        if ch == '\n' {
            (r, w) = (r + 1, 0);
        } else {
            w += cw;
        }
    }
    text.chars().count()
}

/// The cells of `region` between two screen cells (x, y), as (y, first x, last x), in reading order.
fn sel_rows(region: Rect, a: (u16, u16), b: (u16, u16)) -> Vec<(u16, u16, u16)> {
    if region.is_empty() {
        return vec![];
    }
    let clamp = |p: (u16, u16)| (p.0.clamp(region.x, region.right() - 1), p.1.clamp(region.y, region.bottom() - 1));
    let (a, b) = (clamp(a), clamp(b));
    let (s, e) = if (a.1, a.0) <= (b.1, b.0) { (a, b) } else { (b, a) };
    (s.1..=e.1).map(|y| (y, if y == s.1 { s.0 } else { region.x }, if y == e.1 { e.0 } else { region.right() - 1 })).collect()
}

/// While the mouse is down or a selection stands: remember what the screen shows (a wide glyph's second cell is
/// "") for the words and the copy, and paint the selected cells.
fn select(f: &mut Frame, app: &mut App) {
    let Some((a, b)) = app.drag else {
        app.sel_dirty = false;
        return;
    };
    let area = f.area();
    let buf = f.buffer_mut();
    app.snap = (area.y..area.bottom())
        .map(|y| {
            let mut skip = 0;
            (area.x..area.right())
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
    if a != b && std::mem::take(&mut app.sel_dirty) {
        app.pending_copy = Some(selection_text(&app.snap, app.region, a, b)).filter(|t| !t.is_empty());
    }
    if a != b {
        for (y, x0, x1) in sel_rows(app.region, a, b) {
            for x in x0..=x1 {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_style(theme::selection());
                }
            }
        }
    }
}

/// The text between two screen cells, from the cells drawn last, one line per row with trailing blanks cut.
pub fn selection_text(snap: &[Vec<String>], region: Rect, a: (u16, u16), b: (u16, u16)) -> String {
    let lines: Vec<String> = sel_rows(region, a, b)
        .into_iter()
        .map(|(y, x0, x1)| {
            let row = snap.get(y as usize).map_or(&[][..], |r| &r[..]);
            row.get(x0 as usize..=x1 as usize).unwrap_or_default().concat().trim_end().to_string()
        })
        .collect();
    lines.join("\n").trim_end().to_string()
}

/// The run of word characters around column `x` of a drawn row (paths and links count as words), if `x` is on one.
pub fn word_at(row: &[String], x: u16, region: Rect) -> Option<(u16, u16)> {
    let is_word = |i: u16| row.get(i as usize).is_some_and(|s| s.is_empty() || s.chars().next().is_some_and(|c| c.is_alphanumeric() || "_-./:~@#%+=?&".contains(c)));
    if !is_word(x) {
        return None;
    }
    let (mut a, mut b) = (x, x);
    while a > region.x && is_word(a - 1) {
        a -= 1;
    }
    while b + 1 < region.right() && is_word(b + 1) {
        b += 1;
    }
    Some((a, b))
}

/// The panel to show, if any. One at a time; the first that applies wins.
fn surface(app: &mut App, width: usize) -> Option<View> {
    if let Some(p) = app.sessions[app.active].pending.front() {
        return Some(prompt_view(p, width.saturating_sub(4), &app.cwd));
    }
    if let Some(panel) = app.front_panel() {
        return panel_view(app, panel, width);
    }
    let rows = app.completions();
    (!rows.is_empty()).then(|| suggest_view(&rows, app.sel, width))
}

/// What a front panel draws; the explorer is drawn apart, as the whole screen.
fn panel_view(app: &App, panel: crate::Panel, width: usize) -> Option<View> {
    use crate::Panel as P;
    let s = &app.sessions[app.active];
    match panel {
        P::Explorer => None,
        P::Color => app.color_picker.map(color_view),
        P::Model => Some(model_view(app)),
        P::Rewind => app.rewind.map(|p| rewind_view(s, p, width)),
        P::Sched => app.sched_ui.as_ref().map(|ui| {
            let q = ui.target.and_then(|n| s.queue.get(n));
            let gist = q.map(|q| crate::queue::gist(q, width.saturating_sub(16)));
            crate::schedule::view(ui, gist.as_deref(), q.is_some_and(|q| q.due.is_some()), std::time::SystemTime::now())
        }),
        P::Queue => app.queue_ui.as_ref().map(|ui| crate::queue::view(ui, &s.queue, s.queue_hold, s.busy, width)),
        P::Search => app.search.as_ref().map(crate::search::view),
        P::Procs => app.procs_ui.as_ref().map(|ui| crate::procs::view(ui, &app.peers, std::process::id())),
        P::Lanes => app.lanes.as_ref().map(|ui| crate::lanes::view(ui, &crate::lanes::collect(&s.items, &s.shells, s.cwd.as_deref().unwrap_or(&app.cwd)), width)),
        P::Ctx => app.ctx_ui.as_ref().map(|ui| crate::context::view(ui, &s.usage, width)),
        P::Usage => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
            Some(usage::view(&s.usage, width, now))
        }
        P::Opt => app.opt.as_ref().map(|ui| optimize::view(ui, width)),
        P::Perms => app.perms.as_ref().map(|ui| permissions::view(ui, width)),
        P::Instr => app.instr.as_ref().map(|ui| instructions::view(ui, width)),
        P::TabCfg => app.tabcfg.as_ref().map(|ui| tabcfg::view(ui, s, app.cwd(), width)),
        P::Settings => app.settings.as_ref().map(|ui| settings::view(ui, width, |k| app.setting_value(k))),
        P::Worktree => app.wt_ui.as_ref().map(|ui| {
            let canon = |p: &str| std::fs::canonicalize(p).map_or(p.to_string(), |c| c.to_string_lossy().into_owned());
            let tabs: Vec<String> = app.sessions.iter().map(|s| canon(s.cwd.as_deref().unwrap_or(&app.cwd))).collect();
            crate::worktree::view(ui, &tabs, &canon(app.cwd()))
        }),
        P::Open => app.open_ui.as_ref().map(crate::open::view),
        P::Term => app.term_ui.as_ref().map(|ui| crate::termui::view(ui, &s.terms.all(), app.agent_term)),
        P::Supervisor => app.sup_ui.as_ref().map(|ui| {
            let (link, pid) = app.sup_link();
            crate::supui::view(ui, link, pid, &app.peers, std::process::id(), supervisor::exe_mtime(), |i| app.sup_setting(i), width)
        }),
        P::Items => app.items_ui.as_ref().map(|ui| crate::segments::view(ui, &app.line_items)),
    }
}

/// The `/color` choices, each name in its own colour; the UI behind already wears the highlighted one.
fn color_view(sel: usize) -> View {
    let body: Vec<Line> = App::color_choices().into_iter().enumerate().map(|(i, c)| panel::row(i == sel, vec![Span::styled(format!("{} ", theme::CURRENT), theme::tint(Some(c))), Span::raw(c)])).collect();
    let footer = theme::hints(&[("↑↓", "preview"), ("enter", "apply"), ("esc", "cancel")]);
    View::new("Colour", body, sel, footer)
}

/// The model list with the effort levels and fast mode of the highlighted model pinned beneath it.
pub(crate) fn model_view(app: &App) -> View {
    let Some(p) = app.model_picker else { return View::new("", vec![], 0, Line::raw("")) };
    let models = app.model_list();
    let s = &app.sessions[app.active];
    let cur = app.current_model();
    // Four menus, one with the keys at a time: the models and their versions side by side (both vertical),
    // then the effort and fast mode (both horizontal).
    let focus = p.focused(crate::ModelPicker::usable(models, p.sel));
    let fams = crate::session::families(models);
    let at = fams.iter().position(|f| f.1.contains(&p.sel)).unwrap_or(0);
    let versions: Vec<(usize, &str)> = fams.get(at).map_or(vec![], |f| f.1.iter().filter_map(|&i| models[i].parts().1.map(|v| (i, v))).collect());
    let vat = versions.iter().position(|v| v.0 == p.sel).unwrap_or(0);
    let name_w = fams.iter().map(|f| f.0.width()).max().unwrap_or(0).max(5);
    let ver_w = versions.iter().map(|v| v.1.width()).max().unwrap_or(0);
    let cell = |text: String, selected: bool, focused: bool| match (selected, focused) {
        (true, true) => Span::styled(text, theme::selected()),
        (true, false) => Span::styled(text, theme::bold()),
        _ => Span::raw(text),
    };
    let body_len = fams.len().max(versions.len());
    let mut body: Vec<Line> = (0..body_len)
        .map(|r| {
            let mut spans = Vec::new();
            match fams.get(r) {
                Some((family, entries)) => {
                    let (on, mark) = (r == at, if cur.is_some_and(|c| entries.contains(&c)) { theme::CURRENT } else { " " });
                    let cursor = if !on { "  ".to_string() } else if focus == 0 { format!("{} ", theme::CURSOR) } else { format!("{} ", theme::CURSOR_AWAY) };
                    spans.push(Span::styled(cursor, if on && focus == 0 { theme::accent() } else { theme::dim() }));
                    spans.push(cell(format!(" {mark} {family:<name_w$} "), on, focus == 0));
                }
                None => spans.push(Span::raw(" ".repeat(name_w + 6))),
            }
            spans.push(Span::raw("   "));
            if let Some(&(i, v)) = versions.get(r) {
                let mark = if cur == Some(i) { theme::CURRENT } else { " " };
                spans.push(cell(format!(" {mark} {v:<ver_w$} "), r == vat, focus == 1));
            }
            Line::from(spans)
        })
        .collect();
    if body.is_empty() {
        body.push(Line::styled("loading models…", theme::dim()));
    }
    let head = Line::from(vec![Span::raw("  "), panel::menu_label(&format!(" {:<w$}", "model", w = name_w + 4), focus == 0), Span::raw("   "), panel::menu_label(" version", focus == 1)]);
    let mut tail = vec![Line::styled(format!("  {}", models.get(p.sel).map_or("", |m| m.desc.as_str())), theme::dim())];
    if let Some(m) = models.get(p.sel).filter(|m| !m.efforts.is_empty()) {
        let mut spans = vec![Span::raw("  "), panel::menu_label("effort ", focus == 2)];
        for (i, e) in m.efforts.iter().enumerate() {
            spans.push(theme::chip_in(e, *e == s.effort, i == p.effort, focus == 2, theme::bold()));
        }
        tail.push(Line::from(spans));
    }
    // Fast mode: Opus only, so the row says so instead of vanishing and shifting the layout.
    let cur_fast = s.fast_mode || s.fast_pref == Some(true);
    if models.get(p.sel).is_some_and(|m| m.fast) {
        let mut spans = vec![Span::raw("  "), panel::menu_label("fast ↯ ", focus == 3)];
        for (on, label) in [(false, "off"), (true, "on")] {
            spans.push(theme::chip_in(label, cur_fast == on, p.fast == on, focus == 3, theme::bold()));
        }
        spans.push(Span::styled("  faster output, higher price per token", theme::dim()));
        tail.push(Line::from(spans));
    } else {
        tail.push(Line::styled("  fast ↯  not available on this model (Opus only)", theme::dim()));
    }
    let footer = theme::hints(&[("↑↓←→", "move"), ("tab", "next menu"), ("enter", "apply"), ("esc", "cancel")]);
    let row = if focus == 1 { vat } else { at };
    View::new("Model, effort & fast mode", body, row, footer).with_head(vec![head]).with_tail(tail)
}

/// Earlier messages to go back to, with what to restore pinned beneath.
fn rewind_view(s: &crate::session::Session, p: crate::RewindPicker, width: usize) -> View {
    let points = s.rewind_points();
    let sel = p.sel.min(points.len().saturating_sub(1));
    let body: Vec<Line> = points
        .iter()
        .enumerate()
        .map(|(i, (_, t))| panel::row_in(i == sel, !p.on_restore, vec![Span::raw(truncate(&t.split_whitespace().collect::<Vec<_>>().join(" "), width.saturating_sub(8)))]))
        .collect();
    let edited = points.get(sel).is_some_and(|(at, _)| s.edits_since(*at));
    let choices = crate::RewindPicker::choices(edited);
    let mut spans = vec![panel::menu_label("restore  ", p.on_restore)];
    for (i, (label, ..)) in choices.iter().enumerate() {
        spans.push(theme::chip_in(label, false, i == p.restore.min(choices.len() - 1), p.on_restore, theme::bold()));
    }
    if !edited {
        spans.push(Span::styled("  no files edited since", theme::dim()));
    }
    let keys: &[(&str, &str)] = if edited { &[("↑↓←→", "move"), ("tab", "next menu"), ("enter", "rewind"), ("esc", "cancel")] } else { &[("↑↓", "move"), ("enter", "rewind"), ("esc", "cancel")] };
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
    let footer = theme::hints(&[("tab", "complete"), ("↑↓", "move"), ("esc", "hide")]);
    View::new("", body, sel, footer).input_focus().rows(0, 8)
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
            _ if s.busy => tint,
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

/// Put `side` columns of room on both ends of a line; a band (a background at that end) carries its colour into it.
fn side_room(l: &mut Line<'static>, side: usize) {
    let room = |sp: Option<&Span>| Span::styled(" ".repeat(side), sp.and_then(|s| s.style.bg).map_or(Style::new(), |c| Style::new().bg(c)));
    let (left, right) = (room(l.spans.first()), room(l.spans.last()));
    l.spans.insert(0, left);
    l.spans.push(right);
}

fn item_lines(item: &Item, width: u16, side: u16, expanded: bool, cwd: &str) -> Vec<Line<'static>> {
    // Your messages are a band from edge to edge, with `❯` at the very start: no room at the sides.
    let side = if matches!(item.entry, Entry::User(_) | Entry::Rich(_)) { 0 } else { side };
    let mut l = entry_lines(&item.entry, width.saturating_sub(2 * side) as usize, expanded != item.flipped, cwd);
    l.iter_mut().for_each(|l| side_room(l, side as usize));
    l
}

/// A line as one string per screen column (a wide glyph's second column is "", a zero-width mark joins the one before).
fn cells(l: &Line) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in l.spans.iter().flat_map(|s| s.content.chars()) {
        match c.width().unwrap_or(0) {
            0 => out.last_mut().map_or((), |p| p.push(c)),
            w => {
                out.push(c.to_string());
                out.extend((1..w).map(|_| String::new()));
            }
        }
    }
    out
}

/// What the transcript selection covers, one line per row with trailing blanks cut.
fn sel_text(s: &Session, lo: Spot, hi: Spot, width: u16, side: u16, expanded: bool, cwd: &str) -> String {
    let (x_min, x_max) = (s.sel.area.x, s.sel.area.right().saturating_sub(1));
    let mut out = Vec::new();
    for i in lo.0..=hi.0.min(s.items.len().saturating_sub(1)) {
        let item = &s.items[i];
        let kept = item.cache.as_ref().filter(|c| c.0 == width && c.2.len() == c.1 && c.1 > 0);
        let fresh;
        let lines: &[Line] = match kept {
            Some(c) => &c.2,
            None => {
                fresh = item_lines(item, width, side, expanded, cwd);
                &fresh
            }
        };
        let (r0, r1) = (if i == lo.0 { lo.1 } else { 0 }, if i == hi.0 { hi.1 } else { usize::MAX });
        for (r, l) in lines.iter().enumerate().filter(|(r, _)| (r0..=r1).contains(r)) {
            let x0 = if (i, r) == (lo.0, lo.1) { lo.2 } else { x_min };
            let x1 = if (i, r) == (hi.0, hi.1) { hi.2 } else { x_max };
            let row = cells(l);
            let (x0, x1) = ((x0 - x_min) as usize, (x1 - x_min) as usize);
            out.push(row.get(x0..=x1.min(row.len().saturating_sub(1))).unwrap_or_default().concat().trim_end().to_string());
        }
    }
    out.join("\n").trim_matches('\n').trim_end().to_string()
}

fn transcript(f: &mut Frame, s: &mut Session, area: Rect, side: u16, expanded: bool, cwd: &str) -> Vec<(u16, u16, usize)> {
    s.sel.rows.clear();
    s.sel.area = area;
    if s.items.is_empty() {
        s.sel.clear();
        f.render_widget(Paragraph::new(welcome(s, cwd, area.width.saturating_sub(2 * side) as usize)), area.inner(ratatui::layout::Margin::new(side, 0)));
        return Vec::new();
    }
    let width = area.width;
    let h = area.height as usize;
    // `keep: false` counts rows without holding the lines, so measuring a whole session never holds it all.
    let rows = |item: &mut Item, keep: bool| -> usize {
        match &item.cache {
            Some((w, n, _)) if *w == width => *n,
            _ => {
                let mut l = item_lines(item, width, side, expanded, cwd);
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
    let mut top_cut = 0;
    let mut rowmap: Vec<(usize, usize)> = Vec::with_capacity(h);
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
            rowmap.extend((from..).take(lines.len() - at).map(|r| (i, r)));
            if matches!(item.entry, Entry::Tool { nested: false, .. }) {
                calls.push((area.y + at as u16, area.y + lines.len() as u16, i));
            }
            shown.1 = i;
            if at == 0 {
                shown.0 = i;
                top_cut = from;
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
    // The message being answered has scrolled off the top: say which it is. The nearest one at or above the view.
    if h > 2
        && let Some(j) = s.items[..=shown.0].iter().rposition(|it| crate::session::user_said(&it.entry).is_some())
        && (j < shown.0 || top_cut > 1) // row 0 of a message is its blank spacer
    {
        let text = crate::session::user_said(&s.items[j].entry).unwrap_or_default();
        // The band runs edge to edge, as your message does.
        let hint = prompt_hint(&text, area.width as usize);
        f.render_widget(Paragraph::new(hint), Rect::new(area.x, area.y, area.width, 1));
    }
    if s.scroll > 0 && h > 1 {
        let note = theme::keys(&format!(" ↓ {} lines below · ctrl+end ", s.scroll));
        let w = (note.chars().count() as u16).min(area.width);
        f.render_widget(Paragraph::new(Span::styled(note, theme::selected())), Rect::new(area.right() - w, area.bottom() - 1, w, 1));
    }
    s.sel.rows = rowmap;
    selection(f, s, width, side, expanded, cwd);
    calls
}

/// One row naming the message the transcript below answers: `❯ ` and the start of it, on the message band.
fn prompt_hint(text: &str, width: usize) -> Line<'static> {
    let band = theme::user_band();
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let room = width.saturating_sub(2);
    let (mut out, mut used) = (String::new(), 0);
    for c in first.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > room.saturating_sub(1) && first.width() > room {
            out.push('…');
            used += 1;
            break;
        }
        out.push(c);
        used += w;
    }
    out.push_str(&" ".repeat(room.saturating_sub(used)));
    Line::from(vec![Span::styled("❯ ", band.patch(theme::accent()).add_modifier(Modifier::BOLD)), Span::styled(out, band)])
}

/// The transcript selection: follow a pointer held past an edge, widen a double or triple click, read the text when the
/// ends moved, and paint it. Runs after the rows are laid out, so it always agrees with what is on screen.
fn selection(f: &mut Frame, s: &mut Session, width: u16, side: u16, expanded: bool, cwd: &str) {
    let area = s.sel.area;
    if s.sel.a.or(s.sel.b).is_some_and(|p| p.0 >= s.items.len()) {
        return s.sel.clear(); // the entries it was on are gone (/clear, rewind)
    }
    let (first, last) = (s.sel.rows.first().copied(), s.sel.rows.last().copied());
    if s.sel.live && s.sel.edge != 0 && let (Some(top), Some(bottom)) = (first, last) {
        let (i, r) = if s.sel.edge < 0 { top } else { bottom };
        s.sel.b = Some((i, r, if s.sel.edge < 0 { area.x } else { area.right().saturating_sub(1) }));
        s.sel.dirty = true;
    }
    if s.sel.expand != 0 && let Some((i, r, x)) = s.sel.a.filter(|p| p.0 < s.items.len()) {
        let (lo, hi) = if s.sel.expand == 3 {
            (area.x, area.right().saturating_sub(1))
        } else {
            let row: Vec<String> = item_lines(&s.items[i], width, side, expanded, cwd).get(r).map(cells).unwrap_or_default();
            ui_word(&row, x - area.x, area.width).map_or((x, x), |(a, b)| (a + area.x, b + area.x))
        };
        (s.sel.a, s.sel.b, s.sel.expand, s.sel.dirty) = (Some((i, r, lo)), Some((i, r, hi)), 0, true);
    }
    if s.sel.dirty {
        s.sel.dirty = false;
        s.sel.text = s.sel.span().map(|(lo, hi)| sel_text(s, lo, hi, width, side, expanded, cwd)).filter(|t| !t.is_empty());
    }
    let Some((lo, hi)) = s.sel.span() else { return };
    let buf = f.buffer_mut();
    for (k, &(i, r)) in s.sel.rows.iter().enumerate().filter(|(_, p)| (lo.0, lo.1) <= **p && **p <= (hi.0, hi.1)) {
        let x0 = if (i, r) == (lo.0, lo.1) { lo.2 } else { area.x };
        let x1 = if (i, r) == (hi.0, hi.1) { hi.2 } else { area.right().saturating_sub(1) };
        for x in x0..=x1 {
            if let Some(c) = buf.cell_mut((x, area.y + k as u16)) {
                c.set_style(theme::selection());
            }
        }
    }
}

/// The word around column `x` of a row of cells, if it is on one: `word_at` for a row that starts at column 0.
fn ui_word(row: &[String], x: u16, width: u16) -> Option<(u16, u16)> {
    word_at(row, x, Rect::new(0, 0, width, 1))
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

pub(crate) fn entry_lines(e: &Entry, width: usize, expanded: bool, cwd: &str) -> Vec<Line<'static>> {
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
                        // The picture sits on the message's band, so the band does not break around it.
                        let bg = theme::user_band().bg.map_or(Style::new(), |c| Style::new().bg(c));
                        for mut l in shown.iter().flat_map(crate::image::placeholder_lines) {
                            l.spans.insert(0, Span::raw("  "));
                            let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
                            l.spans.push(Span::raw(" ".repeat(width.saturating_sub(w))));
                            l.spans.iter_mut().for_each(|sp| sp.style = sp.style.patch(bg));
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
                    out.push(Line::styled(theme::keys("     ▴ fold (alt+o)"), theme::dim()));
                }
            }
            if let Some(band) = theme::tool_band(name).filter(|_| !*nested) {
                on_band(&mut out[1..], band, width);
            }
        }
        Entry::Note(t) => block(&mut out, t, width, Span::raw("  "), theme::dim()),
        Entry::Error(t) => block(&mut out, t, width, Span::styled("✗ ", theme::danger()), theme::danger()),
        Entry::Side { question, answer, .. } => side_lines(&mut out, question, answer, width),
        Entry::Image(s) => out.extend(crate::image::placeholder_lines(s)),
    }
    out
}

/// Lay `band` behind `lines`, the full width: only where a span has no background (a diff line keeps its own)
/// and no colour (the band's text colour, so nothing turns invisible on it).
fn on_band(lines: &mut [Line<'static>], band: Style, width: usize) {
    for l in lines {
        let w: usize = l.spans.iter().map(|s| s.content.width()).sum();
        l.spans.push(Span::raw(" ".repeat(width.saturating_sub(w))));
        for sp in &mut l.spans {
            sp.style.bg = sp.style.bg.or(band.bg);
            sp.style.fg = sp.style.fg.or(band.fg);
        }
    }
}

/// A `/btw`: the question under a `btw` label, then the answer in a quiet gutter so it cannot be taken for
/// part of the conversation, and what it cost.
fn side_lines(out: &mut Vec<Line<'static>>, question: &str, answer: &Option<Result<(String, f64), String>>, width: usize) {
    block(out, question, width, Span::styled("  btw ", theme::accent().add_modifier(Modifier::BOLD)), Style::new());
    let gutter = || Span::styled("  │ ", theme::dim());
    match answer {
        None => out.push(Line::from(vec![gutter(), Span::styled("asking… (not added to the conversation)", theme::dim())])),
        Some(Err(e)) => block(out, e, width, Span::styled("  ✗ ", theme::danger()), theme::danger()),
        Some(Ok((text, cost))) => {
            for mut l in crate::markdown::render(text, width.saturating_sub(4)) {
                l.spans.insert(0, gutter());
                out.push(l);
            }
            out.push(Line::from(vec![gutter(), Span::styled(format!("${cost:.4} · not in the conversation"), theme::dim())]));
        }
    }
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
    let look = crate::motion::look();
    let what = if s.activity.is_empty() { "working" } else { &s.activity };
    let mut what: Vec<char> = what.chars().collect();
    what[0] = what[0].to_ascii_uppercase();
    let mut detail = crate::session::duration(el);
    if s.turn_chars >= 4 {
        detail += &format!(" · ↓ {} tokens", crate::usage::tokens((s.turn_chars / 4) as u64));
    }
    let done = s.tasks.iter().filter(|t| t.2 == "completed").count();
    if done < s.tasks.len() {
        detail += &format!(" · {}", crate::motion::count_bar(done, s.tasks.len(), 5));
    }
    let mut head: Vec<Span<'static>> = look.frame(el).map(|f| Span::styled(format!("{f} "), theme::working())).into_iter().collect();
    head.extend(look.shimmer(&format!("{}…", what.into_iter().collect::<String>()), theme::working().add_modifier(Modifier::BOLD), theme::working_glint(), el));
    let tail = Span::styled(theme::keys(&format!(" ({detail} · esc to interrupt{})", if s.shells.iter().any(|x| x.status.is_none() && !x.background) { " · ctrl+b to background" } else { "" })), theme::dim());
    let line = Line::from(head.iter().cloned().chain([tail]).collect::<Vec<_>>());
    if line.width() > width { Line::from(head) } else { line }
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
    fn a_model_name_splits_into_family_and_version() {
        assert_eq!(crate::session::split_name("Sonnet 5.5"), ("Sonnet", Some("5.5")));
        assert_eq!(crate::session::split_name("Opus 4"), ("Opus", Some("4")));
        assert_eq!(crate::session::split_name("Default (recommended)"), ("Default (recommended)", None));
        assert_eq!(crate::session::split_name("Haiku"), ("Haiku", None));
        assert_eq!(crate::session::split_name("Mix 5.5b"), ("Mix 5.5b", None));
    }

    #[test]
    fn commands_and_file_calls_sit_on_one_full_width_band_and_other_tools_do_not() {
        let entry = |name: &str, input: Value, result: &str| Entry::Tool { id: "t".into(), name: name.into(), input, result: Some((result.into(), false)), nested: false };
        let lines = |e: &Entry| entry_lines(e, 40, false, "/p");
        let bash = lines(&entry("Bash", serde_json::json!({"command": "ls"}), "a\nb"));
        assert!(bash[0].spans.iter().all(|s| s.style.bg.is_none()), "the blank line above the call is not part of it");
        for l in &bash[1..] {
            assert_eq!(l.spans.iter().map(|s| s.content.width()).sum::<usize>(), 40, "full width: {l}");
            assert!(l.spans.iter().all(|s| s.style.bg.is_some() && s.style.fg.is_some()), "{l}");
        }
        let edit = lines(&entry("Edit", serde_json::json!({"file_path": "/p/a.rs", "old_string": "x", "new_string": "y"}), "ok"));
        let band = theme::tool_band("Edit").unwrap().bg;
        let changed: Vec<_> = edit.iter().filter(|l| l.to_string().contains("+ y") || l.to_string().contains("- x")).collect();
        assert_eq!(changed.len(), 2);
        assert!(changed.iter().all(|l| l.spans.iter().any(|s| s.style.bg.is_some() && s.style.bg != band)), "a diff line keeps its green or red");
        assert!(edit[1].spans.iter().all(|s| s.style.bg == band), "the header is on the band");
        for name in ["Read", "Write", "Glob", "Grep", "NotebookEdit"] {
            assert!(theme::tool_band(name).is_some(), "{name}");
        }
        let other = lines(&entry("mcp__x__y", serde_json::json!({}), "ok"));
        assert!(other.iter().flat_map(|l| &l.spans).all(|s| s.style.bg.is_none()));
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
    assert!(s.elapsed() < std::time::Duration::from_secs(1), "{:?}", s.elapsed());
}

#[cfg(test)]
#[test]
fn a_band_runs_into_the_side_room_and_plain_text_leaves_it_blank() {
    let mut band = Vec::new();
    user_lines(&mut band, "hi", 20, true);
    let mut plain = Line::raw("text");
    side_room(&mut band[0], 2);
    side_room(&mut plain, 2);
    let bg = theme::user_band().bg;
    assert!(bg.is_some());
    assert_eq!((band[0].spans[0].style.bg, band[0].spans.last().unwrap().style.bg), (bg, bg));
    assert_eq!((plain.spans[0].style.bg, plain.spans.last().unwrap().style.bg), (None, None));
}
