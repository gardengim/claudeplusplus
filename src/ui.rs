//! Rendering. Each transcript entry is wrapped once per width and cached, and only
//! the visible window of lines is handed to ratatui, so redraw cost stays flat as
//! the conversation grows.

use crate::panel::{self, View};
use crate::{App, instructions, optimize, permissions, settings, tabcfg, theme, usage};
use crate::session::{Entry, Part, Pending, Session};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const COLLAPSED_LINES: usize = 4;

/// The docked list needs a screen at least this wide, and takes this many columns.
const DOCK_MIN_SCREEN: u16 = 100;
const DOCK_W: u16 = 30;

pub fn draw(f: &mut Frame, app: &mut App) {
    // The full-screen session list replaces everything: no tabs, input or frame.
    if let Some(ex) = app.explorer.as_mut() {
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
        if let Some(d) = app.dock.as_mut() {
            let lines = d.dock_lines(DOCK_W as usize - 1, dock.height as usize, focused);
            f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::RIGHT).border_style(if focused { theme::accent() } else { theme::dim() })), dock);
        }
        area = rest;
    }
    let input_w = area.width.saturating_sub(2).max(1) as usize;
    let (input_lines, cursor) = input_layout(&app.input, app.cursor, input_w);
    // Whatever panel is open (a prompt, a picker, config, suggestions...) is built as a `View`, sized and
    // drawn by the one shared panel renderer, at the bottom of the screen above the input.
    let view = surface(app, area.width as usize);
    let panel_h = view.as_ref().map_or(0, |v| panel::height(v, area.height));
    let input_keeps_focus = view.as_ref().is_none_or(|v| v.input_focus);
    let s = &app.sessions[app.active];
    let (input_rows, cursor_row) = input_rows(&input_lines, cursor.1, &s.attachments);
    // The user's own status line (if configured and it has produced output) sits under the input.
    // Like Claude Code, it steps aside while a permission prompt or question is up, and on tiny terminals.
    let ext_h = if app.statusline_on && s.pending.is_empty() && area.height >= 14 {
        s.status_lines.len().min(crate::statusline::MAX_LINES) as u16
    } else {
        0
    };
    let [tabs, body, panel_area, input_area, ext_area, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(panel_h),
        Constraint::Length(input_rows.len().min((area.height / 2).max(8) as usize) as u16 + 1),
        Constraint::Length(ext_h),
        Constraint::Length(1),
    ])
    .areas(area);
    if ext_h > 0 {
        f.render_widget(Paragraph::new(s.status_lines[..ext_h as usize].to_vec()), ext_area);
    }

    f.render_widget(Paragraph::new(tab_line(app)), tabs);
    let expanded = app.expanded;
    let cwd = app.cwd.clone();
    transcript(f, &mut app.sessions[app.active], body, expanded, &cwd);
    if let Some(view) = view.filter(|_| panel_h > 0) {
        panel::render(f, panel_area, view);
    }

    // Input box: a top rule plus the wrapped text, scrolled to keep the cursor visible.
    let visible = input_area.height.saturating_sub(1) as usize;
    let skip = (cursor_row + 1).saturating_sub(visible);
    let text: Vec<Line> = if app.input.is_empty() {
        vec![Line::from(vec![Span::raw("> "), Span::styled(app.placeholder(), theme::dim())])]
    } else {
        input_rows.into_iter().skip(skip).collect()
    };
    let rule = Block::new().borders(Borders::TOP).border_style(theme::dim());
    f.render_widget(Paragraph::new(text).block(rule), input_area);
    // A panel that owns the keyboard also owns the cursor: leave it hidden.
    if input_keeps_focus {
        f.set_cursor_position(Position::new(input_area.x + 2 + cursor.0 as u16, input_area.y + 1 + (cursor_row - skip) as u16));
    }
    f.render_widget(Paragraph::new(status_line(app, area.height)), status);
}

/// The panel to show, if any. One at a time; the first that applies wins.
fn surface(app: &mut App, width: usize) -> Option<View> {
    if let Some(p) = app.sessions[app.active].pending.front() {
        return Some(prompt_view(p, width.saturating_sub(4), &app.cwd));
    }
    if app.model_picker.is_some() {
        return Some(model_view(app));
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
    if let Some(ui) = app.settings {
        return Some(settings::view(ui, width, |k| app.setting_value(k)));
    }
    let rows = app.completions();
    (!rows.is_empty()).then(|| suggest_view(&rows, app.sel, width))
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

fn tab_line(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
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
        let style = if i == app.active { theme::selected() } else { theme::dim() };
        let mark_style = match (i == app.active, mark) {
            (true, _) => style,
            (_, "◆") => theme::attention(),
            (_, "◐") => theme::accent(),
            _ => style,
        };
        spans.push(Span::styled(format!(" {}", i + 1), style));
        spans.push(Span::styled(mark.to_string(), mark_style));
        spans.push(Span::styled(format!("{title} "), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn transcript(f: &mut Frame, s: &mut Session, area: Rect, expanded: bool, cwd: &str) {
    let width = area.width;
    let mut total = 0;
    for item in &mut s.items {
        if item.cache.as_ref().map(|c| c.0) != Some(width) {
            item.cache = Some((width, entry_lines(&item.entry, width as usize, expanded, cwd)));
        }
        total += item.cache.as_ref().unwrap().1.len();
    }
    let h = area.height as usize;
    // Scrolled up: keep the same lines in view while new output arrives below.
    if s.scroll > 0 && total > s.last_total {
        s.scroll += total - s.last_total;
    }
    (s.last_total, s.view_h) = (total, h);
    // `/find` asked for an entry: put its first line at the top of the view.
    if let Some(i) = s.reveal.take() {
        let above: usize = s.items[..i.min(s.items.len())].iter().map(|it| it.cache.as_ref().map_or(0, |c| c.1.len())).sum();
        s.scroll = total.saturating_sub(h + above.saturating_sub(1));
    }
    s.scroll = s.scroll.min(total.saturating_sub(h));
    let start = total.saturating_sub(h + s.scroll);
    let mut lines = Vec::with_capacity(h);
    let mut pos = 0;
    for item in &s.items {
        let l = &item.cache.as_ref().unwrap().1;
        if pos + l.len() > start {
            let from = start.saturating_sub(pos);
            lines.extend(l[from..].iter().take(h - lines.len()).cloned());
            if lines.len() >= h {
                break;
            }
        }
        pos += l.len();
    }
    f.render_widget(Paragraph::new(lines), area);
    if s.scroll > 0 && h > 1 {
        let note = format!(" ↓ {} lines below · ctrl+end ", s.scroll);
        let w = (note.chars().count() as u16).min(area.width);
        f.render_widget(Paragraph::new(Span::styled(note, theme::selected())), Rect::new(area.right() - w, area.bottom() - 1, w, 1));
    }
}

fn entry_lines(e: &Entry, width: usize, expanded: bool, cwd: &str) -> Vec<Line<'static>> {
    let mut out = vec![Line::raw("")];
    match e {
        Entry::User(t) => block(&mut out, t, width, Span::styled("> ", theme::accent().add_modifier(Modifier::BOLD)), theme::accent()),
        Entry::Rich(parts) => {
            let mut first = true;
            for part in parts {
                match part {
                    Part::Text(t) => {
                        let lead = if first { Span::styled("> ", theme::accent().add_modifier(Modifier::BOLD)) } else { Span::raw("  ") };
                        block(&mut out, t, width, lead, theme::accent());
                        first = false;
                    }
                    Part::Image(n, shown) => {
                        let lead = if first { Span::styled("> ", theme::accent().add_modifier(Modifier::BOLD)) } else { Span::raw("  ") };
                        out.push(Line::from(vec![lead, Span::styled(crate::image::token(*n), theme::accent())]));
                        first = false;
                        for mut l in shown.iter().flat_map(crate::image::placeholder_lines) {
                            l.spans.insert(0, Span::raw("  "));
                            out.push(l);
                        }
                    }
                }
            }
        }
        Entry::Assistant { text, streaming, .. } => {
            let text = if text.is_empty() && *streaming { "…" } else { text };
            markdown(&mut out, text, width);
        }
        Entry::Tool { name, input, result, nested, .. } => tool_lines(&mut out, name, input, result, *nested, width, expanded, cwd),
        Entry::Note(t) => block(&mut out, t, width, Span::raw("  "), theme::dim()),
        Entry::Error(t) => block(&mut out, t, width, Span::styled("✗ ", theme::danger()), theme::danger()),
        Entry::Image(s) => out.extend(crate::image::placeholder_lines(s)),
    }
    out
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
    // still running: receded; finished: green or red. (Yellow is reserved for "waiting on you".)
    let role = match result {
        None => theme::dim(),
        Some((_, true)) => theme::danger(),
        Some(_) => theme::ok(),
    };
    let pad = if nested { "    " } else { "" };
    let summary = tool_summary(name, input, cwd);
    let (lead, text) = match tool_icon(name) {
        Some(icon) => (format!("{pad}{}{icon} ", if nested { "↳ " } else { "" }), summary),
        None => (format!("{pad}{}", if nested { "↳ " } else { "● " }), format!("{name}({summary})")),
    };
    let text = truncate(&text, width.saturating_sub(lead.width()));
    let icon_style = if tool_icon(name).is_some() { theme::tool_hue(name) } else { role }.add_modifier(Modifier::BOLD);
    out.push(Line::from(vec![Span::styled(lead, icon_style), Span::styled(text, role.add_modifier(Modifier::BOLD))]));
    if nested {
        out.remove(0); // subagent steps render as a compact list
        return;
    }
    let limit = if expanded { usize::MAX } else { COLLAPSED_LINES * 3 };
    let mut diff: Vec<(char, &str)> = Vec::new();
    match name {
        "Edit" => {
            let edits: Vec<&Value> = match input["edits"].as_array() {
                Some(a) => a.iter().collect(),
                None => vec![input],
            };
            for e in edits {
                diff.extend(e["old_string"].as_str().unwrap_or_default().lines().map(|l| ('-', l)));
                diff.extend(e["new_string"].as_str().unwrap_or_default().lines().map(|l| ('+', l)));
            }
        }
        "Write" => diff.extend(input["content"].as_str().unwrap_or_default().lines().map(|l| ('+', l))),
        _ => {}
    }
    let n = diff.len();
    for (sign, l) in diff.into_iter().take(limit) {
        let style = if sign == '-' { theme::danger() } else { theme::ok() };
        out.push(Line::from(vec![Span::raw("    "), Span::styled(truncate(&format!("{sign} {l}"), width - 4), style)]));
    }
    if n > limit {
        out.push(Line::styled(format!("    … +{} lines (ctrl+o to expand)", n - limit), theme::dim()));
    }
    let Some((text, err)) = result else { return };
    if n > 0 && !err {
        return; // the diff already says what happened
    }
    let style = if *err { theme::danger() } else { theme::dim() };
    let text = if text.trim().is_empty() { "(no output)" } else { text.trim_end() };
    let lines: Vec<String> = wrap(text, width.saturating_sub(5));
    let limit = if expanded { usize::MAX } else { COLLAPSED_LINES };
    for (i, l) in lines.iter().take(limit).enumerate() {
        let lead = if i == 0 { "  ⎿  " } else { "     " };
        out.push(Line::from(vec![Span::raw(lead), Span::styled(l.clone(), style)]));
    }
    if lines.len() > limit {
        out.push(Line::styled(format!("     … +{} lines (ctrl+o to expand)", lines.len() - limit), theme::dim()));
    }
}

/// One glyph per kind of tool, so a run of tool calls reads by shape. Unknown (MCP...) tools keep their name.
fn tool_icon(name: &str) -> Option<&'static str> {
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
        _ => String::new(),
    };
    let v = if v.is_empty() { input.to_string() } else { v };
    truncate(&v.replace('\n', " ⏎ "), 200)
}

/// What the panel for the prompt at the front of the queue shows.
fn prompt_view(p: &Pending, width: usize, cwd: &str) -> View {
    if p.is_question() {
        return p.ui.question_view(p.questions(), width);
    }
    let headline = format!("Allow {}({})?", p.tool, tool_summary(&p.tool, &p.input, cwd));
    let has_always = p.suggestions.as_array().is_some_and(|a| !a.is_empty());
    p.ui.permission_view(&headline, p.input["plan"].as_str(), has_always, width)
}

fn status_line(app: &App, f_height: u16) -> Line<'static> {
    let s = &app.sessions[app.active];
    // With the user's status line showing, ours keeps only what theirs cannot know: no model, context,
    // cost or limits (their script has those), but the prompt-cache clock, memory and mode.
    let ext = app.statusline_on && !s.status_lines.is_empty() && s.pending.is_empty() && f_height >= 14;
    let mut spans = Vec::new();
    if s.busy {
        let el = s.busy_since.elapsed();
        let frame = SPINNER[(el.as_millis() / 100) as usize % SPINNER.len()];
        let what = if s.activity.is_empty() { "working" } else { &s.activity };
        spans.push(Span::styled(format!("{frame} {what} {}s · ⎋ stop  ", el.as_secs()), theme::bold()));
    }
    let mode_style = theme::mode(&s.mode);
    let mode = match s.mode.as_str() {
        "" | "default" => None,
        "acceptEdits" => Some("⏵⏵ edits"),
        "plan" => Some("◈ plan"),
        "bypassPermissions" => Some("! bypass"),
        m => Some(m),
    };
    if let Some(m) = mode {
        spans.push(Span::styled(format!("{m}  "), mode_style));
    }
    let mut parts: Vec<(String, Style)> = vec![];
    if !s.model.is_empty() && !ext {
        let base = if s.effort.is_empty() { s.model.clone() } else { format!("{} {}", s.model, s.effort) };
        parts.push((if s.fast_mode { format!("{base} ↯") } else { base }, theme::accent()));
    }
    if s.ctx_tokens > 0 && !ext {
        // the context filling up is the one number worth a warning colour
        let hot = s.ctx_tokens * 100 >= s.ctx_window.max(200_000) * 85;
        parts.push((format!("ctx {}", human(s.ctx_tokens)), if hot { theme::danger() } else { theme::dim() }));
    }
    if s.prompt_total > 0 {
        parts.push((format!("hit {:.0}%", 100.0 * s.cache_read as f64 / s.prompt_total as f64), theme::ok()));
    }
    if let Some(left) = s.cache_left(app.cache_ttl) {
        parts.push(if left.is_zero() { ("◷ cold".into(), theme::danger()) } else { (format!("◷ {}m", left.as_secs().div_ceil(60)), theme::dim()) });
    }
    if !s.profile.is_empty() && s.profile != "full" {
        parts.insert(0, (s.profile.clone(), theme::mode("acceptEdits")));
    }
    if s.cost > 0.0 && !ext {
        parts.push((format!("${:.3}", s.cost), theme::ok()));
    }
    if !s.rate.is_empty() && !ext {
        parts.push((s.rate.clone(), theme::dim()));
    }
    let backends: u64 = app.sessions.iter().map(|s| s.mem_kb).sum();
    parts.push((format!("mem {}M+{}M", backends / 1024, app.ui_mem_kb / 1024), theme::dim()));
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

fn human(n: u64) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

pub fn truncate(s: &str, width: usize) -> String {
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
    for para in text.split('\n') {
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
fn input_rows(lines: &[String], cursor_line: usize, atts: &[crate::session::Attachment]) -> (Vec<Line<'static>>, usize) {
    let mut rows = Vec::with_capacity(lines.len());
    let mut cursor_row = 0;
    for (i, l) in lines.iter().enumerate() {
        if i == cursor_line {
            cursor_row = rows.len();
        }
        rows.push(Line::from(format!("{}{l}", if i == 0 { "> " } else { "  " })));
        let shown = crate::image::token_n(l.trim()).and_then(|n| atts.iter().find(|a| a.n == n)).and_then(|a| a.preview);
        for mut p in shown.iter().flat_map(crate::image::placeholder_lines) {
            p.spans.insert(0, Span::raw("  "));
            rows.push(p);
        }
    }
    (rows, cursor_row)
}

fn input_layout(text: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let mut lines = vec![String::new()];
    let mut w = 0;
    let mut pos = (0, 0);
    for (i, ch) in text.chars().enumerate() {
        if i == cursor {
            pos = (w, lines.len() - 1);
        }
        if ch == '\n' {
            lines.push(String::new());
            w = 0;
            continue;
        }
        let cw = ch.width().unwrap_or(0);
        if w + cw > width {
            lines.push(String::new());
            w = 0;
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
    fn wrap_words_and_wide_chars() {
        assert_eq!(wrap("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap("a\n\nb", 5), vec!["a", "", "b"]);
        assert_eq!(wrap("안녕하세요", 4), vec!["안녕", "하세", "요"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("    indented code", 40), vec!["    indented code"]);
    }

    #[test]
    fn input_cursor_tracks_wraps_and_newlines() {
        assert_eq!(input_layout("abc", 3, 10).1, (3, 0));
        assert_eq!(input_layout("ab\ncd", 3, 10).1, (0, 1));
        assert_eq!(input_layout("abcd", 4, 4), (vec!["abcd".into(), "".into()], (0, 1)));
        assert_eq!(input_layout("한글", 1, 10).1, (2, 0));
    }

    #[test]
    fn edit_tool_shows_diff_not_result() {
        let input = serde_json::json!({"file_path": "/p/a.rs", "old_string": "x", "new_string": "y\nz"});
        let mut out = Vec::new();
        tool_lines(&mut out, "Edit", &input, &Some(("ok".into(), false)), false, 80, false, "/p");
        let text: Vec<String> = out.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["± a.rs", "    - x", "    + y", "    + z"]);
    }
}
