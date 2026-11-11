//! `/context`: where this tab's context window goes, drawn. It reads what Claude Code itself reports
//! (`get_context_usage`, the same free request `/usage` makes), so nothing here costs tokens.
//!
//! - A gauge for the whole window (how full, and the room held back for auto-compact), and under it one bar
//!   for how the used part divides between the categories. Both are stacked bars with eighth-block edges, so a
//!   segment smaller than a character still shows.
//! - One row per category; the cursor row picks what the detail shows: which skills, files, tools or messages
//!   the tokens are. On a wide screen the window is also drawn as a grid of squares, the cursor row's squares
//!   enlarged, and the detail sits beside the list; on a narrow one a short detail goes under it.
//! - Enter does the one useful thing for a row where claude++ has it (skills: `/optimize`, memory: `/instructions`).

use crate::panel::{SIDE_MIN_WIDTH, View, cycle, row_with};
use crate::theme;
use crate::ui::{tilde, wrap};
use crate::usage::{Ctx, Usage, tokens};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::BTreeMap;

// ---- what Claude Code reports beyond the category totals ----

/// One request's breakdown of the conversation's own tokens.
#[derive(Default, Clone)]
pub struct Msg {
    pub user: u64,
    pub assistant: u64,
    pub calls: u64,
    pub results: u64,
    pub attachments: u64,
    pub redirected: u64,
    /// (tool, call tokens, result tokens)
    pub tools: Vec<(String, u64, u64)>,
    /// (kind of attachment, tokens)
    pub kinds: Vec<(String, u64)>,
}

#[derive(Default, Clone)]
pub struct Detail {
    /// Category ids, in the order of `Ctx::cats`.
    pub ids: Vec<String>,
    /// Where auto-compact starts, in tokens; 0 when unknown.
    pub threshold: u64,
    pub auto: bool,
    /// (path, type, tokens)
    pub memory: Vec<(String, String, u64)>,
    /// (server, tokens, loaded) for each tool.
    pub mcp: Vec<(String, u64, bool)>,
    /// (agent type, source, tokens)
    pub agents: Vec<(String, String, u64)>,
    /// (name, source, tokens)
    pub skills: Vec<(String, String, u64)>,
    pub msg: Msg,
    /// The last request: input, output, cache written, cache read.
    pub last: Option<[u64; 4]>,
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

pub fn parse(r: &Value) -> Detail {
    let n = crate::backend::count;
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let (mb, u) = (&r["messageBreakdown"], &r["apiUsage"]);
    Detail {
        ids: arr(&r["categories"]).iter().map(|c| s(&c["id"])).collect(),
        threshold: n(&r["autoCompactThreshold"]),
        auto: r["isAutoCompactEnabled"].as_bool().unwrap_or(true),
        memory: arr(&r["memoryFiles"]).iter().map(|m| (s(&m["path"]), s(&m["type"]), n(&m["tokens"]))).collect(),
        mcp: arr(&r["mcpTools"]).iter().map(|m| (s(&m["serverName"]), n(&m["tokens"]), m["isLoaded"].as_bool().unwrap_or(false))).collect(),
        agents: arr(&r["agents"]).iter().map(|a| (s(&a["agentType"]), s(&a["source"]), n(&a["tokens"]))).collect(),
        skills: arr(&r["skills"]["skillFrontmatter"]).iter().map(|k| (s(&k["name"]), s(&k["source"]), n(&k["tokens"]))).collect(),
        msg: Msg {
            user: n(&mb["userMessageTokens"]),
            assistant: n(&mb["assistantMessageTokens"]),
            calls: n(&mb["toolCallTokens"]),
            results: n(&mb["toolResultTokens"]),
            attachments: n(&mb["attachmentTokens"]),
            redirected: n(&mb["redirectedContextTokens"]),
            tools: arr(&mb["toolCallsByType"]).iter().map(|t| (s(&t["name"]), n(&t["callTokens"]), n(&t["resultTokens"]))).collect(),
            kinds: arr(&mb["attachmentsByType"]).iter().map(|t| (s(&t["name"]), n(&t["tokens"]))).collect(),
        },
        last: u.is_object().then(|| [n(&u["input_tokens"]), n(&u["output_tokens"]), n(&u["cache_creation_input_tokens"]), n(&u["cache_read_input_tokens"])]),
    }
}

// ---- categories ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Used,
    /// Not in the request until the model asks for it.
    Deferred,
    /// Held back so auto-compact has room to run.
    Buffer,
    Free,
}

struct Cat {
    id: String,
    name: String,
    tokens: u64,
    kind: Kind,
    slot: usize,
}

impl Cat {
    fn style(&self) -> Style {
        if self.kind == Kind::Used { Style::new().fg(theme::series(self.slot)) } else { theme::dim() }
    }
}

/// A fixed colour for the categories Claude Code has always had; any other gets one by position.
fn slot(id: &str, i: usize) -> usize {
    match id {
        "system_prompt" => 1,
        "system_tools" => 7,
        "skills" => 3,
        "messages" => 2,
        "memory_files" => 5,
        "custom_agents" => 6,
        "slash_commands" => 0,
        s if s.starts_with("mcp") => 4,
        _ => i,
    }
}

/// The non-empty categories: what is used, largest first, then what is deferred, the buffer, and the free space.
fn cats(c: &Ctx) -> Vec<Cat> {
    let mut v: Vec<Cat> = c
        .cats
        .iter()
        .enumerate()
        .filter(|(_, x)| x.1 > 0)
        .map(|(i, (name, t, kind))| {
            let id = c.detail.ids.get(i).cloned().unwrap_or_else(|| name.to_lowercase().replace(' ', "_"));
            let kind = match kind.as_str() {
                "deferred" => Kind::Deferred,
                "buffer" => Kind::Buffer,
                "free" => Kind::Free,
                _ => Kind::Used,
            };
            Cat { slot: slot(&id, i), id, name: name.trim_end_matches(" (deferred)").to_string(), tokens: *t, kind }
        })
        .collect();
    v.sort_by_key(|k| (k.kind as u8, Reverse(k.tokens)));
    v
}

pub fn count(c: &Ctx) -> usize {
    cats(c).len()
}

/// Enter on a row: the command that does something about it, and what to call it.
pub fn action(c: &Ctx, sel: usize) -> Option<(&'static str, &'static str)> {
    match cats(c).get(sel)?.id.as_str() {
        "skills" => Some(("/optimize", "optimize")),
        "memory_files" => Some(("/instructions", "instructions")),
        _ => None,
    }
}

// ---- stacked bars and the grid ----

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fill {
    Solid(Color),
    Free,
    Reserved,
}

/// Each weight's share of `units`, out of `total`: by cumulative rounding, so the parts add up, and at least
/// one unit for anything above zero so a small category is never invisible.
fn share(weights: &[u64], total: u64, units: usize) -> Vec<usize> {
    share_by(weights, total, units, &|_| true)
}

/// [`share`], with the one-unit minimum only for the parts `keep` names.
fn share_by(weights: &[u64], total: u64, units: usize, keep: &dyn Fn(usize) -> bool) -> Vec<usize> {
    let total = total.max(1) as f64;
    let (mut cum, mut prev) = (0u64, 0usize);
    weights
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            cum += w;
            let end = ((cum as f64 / total) * units as f64).round() as usize;
            // leave a unit for every later part that has something in it
            let later = (i + 1..weights.len()).filter(|&j| weights[j] > 0 && keep(j)).count();
            let end = if w > 0 && keep(i) { end.max(prev + 1) } else { end.max(prev) }.min(units.saturating_sub(later)).max(prev);
            let n = end - prev;
            prev = end;
            n
        })
        .collect()
}

/// A bar `width` characters wide from `segs` (fill, eighths of a character), which add up to `width * 8`.
/// Where two segments meet inside a character it is drawn as a partial block: left colour in front, right
/// colour behind, so edges are exact to an eighth.
fn stack(segs: &[(Fill, usize)], width: usize) -> Vec<Span<'static>> {
    const LEFT: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let whole = |f: Fill| match f {
        Fill::Solid(c) => Span::styled("█", Style::new().fg(c)),
        Fill::Free => Span::styled("░", theme::dim()),
        Fill::Reserved => Span::styled("▒", theme::dim()),
    };
    let (mut at, mut used) = (0, 0);
    (0..width)
        .map(|_| {
            let mut pieces: Vec<(Fill, usize)> = vec![];
            let mut need = 8;
            while need > 0 && at < segs.len() {
                let take = (segs[at].1 - used).min(need);
                if take > 0 {
                    pieces.push((segs[at].0, take));
                }
                (used, need) = (used + take, need - take);
                if used == segs[at].1 {
                    (at, used) = (at + 1, 0);
                }
            }
            match (pieces.first().copied(), pieces.last().copied()) {
                (None, _) | (_, None) => Span::raw(" "),
                (Some((f, _)), Some((l, _))) if pieces.len() == 1 || f == l => whole(f),
                (Some((Fill::Solid(a), n)), Some((Fill::Solid(b), _))) => Span::styled(LEFT[n.clamp(1, 7) - 1], Style::new().fg(a).bg(b)),
                // Colour then free or reserved: the right part stays the terminal's own background.
                (Some((Fill::Solid(a), n)), _) => Span::styled(LEFT[n.clamp(1, 7) - 1], Style::new().fg(a)),
                (Some((_, n)), Some((l, _))) => if n >= 4 { whole(Fill::Free) } else { whole(l) },
            }
        })
        .collect()
}

fn fg(s: Style) -> Color {
    s.fg.unwrap_or(Color::Reset)
}

/// The window as rows of squares: used categories first (largest first), then free space, and the buffer in
/// the last cells. The cursor row's squares are drawn larger.
fn grid(cs: &[Cat], max: u64, sel: &str, rows: usize, cols: usize) -> Vec<Line<'static>> {
    let n = rows * cols;
    let used: Vec<&Cat> = cs.iter().filter(|c| c.kind == Kind::Used).collect();
    let buf = cs.iter().find(|c| c.kind == Kind::Buffer);
    let buf_n = buf.map_or(0, |b| share(&[b.tokens], max, n)[0]);
    let mut cells: Vec<Span<'static>> = vec![];
    // Squares are to scale; only the cursor row is guaranteed one, so it can always be found.
    let per = share_by(&used.iter().map(|c| c.tokens).collect::<Vec<_>>(), max, n, &|i| used[i].id == sel);
    for (c, k) in used.iter().zip(per) {
        let mark = if c.id == sel { Span::styled("█", c.style().add_modifier(Modifier::BOLD)) } else { Span::styled("■", c.style()) };
        cells.extend(std::iter::repeat_n(mark, k));
    }
    cells.truncate(n - buf_n);
    let free = if sel == cs.iter().find(|c| c.kind == Kind::Free).map_or("", |c| c.id.as_str()) { "○" } else { "·" };
    cells.resize(n - buf_n, Span::styled(free, theme::dim()));
    let reserved = if buf.is_some_and(|b| b.id == sel) { "▓" } else { "▒" };
    cells.resize(n, Span::styled(reserved, theme::dim()));
    cells.chunks(cols).map(|row| Line::from(row.iter().flat_map(|c| [c.clone(), Span::raw(" ")]).collect::<Vec<_>>())).collect()
}

// ---- the detail for one category ----

fn source_name(s: &str) -> &str {
    match s {
        "userSettings" => "yours",
        "plugin" => "plugins",
        "syncedSkills" => "claude.ai",
        s => s,
    }
}

/// What a row stands for and what makes up its tokens; `w` is the width to fit.
fn detail(c: &Ctx, cat: &Cat, w: usize) -> Vec<Line<'static>> {
    let (d, w) = (&c.detail, w.max(24));
    let row = |label: String, t: u64| Line::from(vec![Span::raw(format!("{:<n$}", crate::ui::truncate(&label, w - 8), n = w - 7)), Span::styled(format!("{:>6}", tokens(t)), theme::bold())]);
    let sub = |s: &str| Line::styled(s.to_string(), theme::dim());
    let note = |s: &str| wrap(s, w).into_iter().map(|l| Line::styled(l, theme::dim())).collect::<Vec<_>>();
    let mut out = vec![Line::from(vec![Span::styled("■ ", cat.style()), Span::styled(cat.name.clone(), theme::bold()), Span::styled(format!("  {}", tokens(cat.tokens)), theme::dim())])];
    match cat.id.as_str() {
        "skills" => {
            let mut by: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
            for (_, src, t) in &d.skills {
                let e = by.entry(source_name(src)).or_default();
                (e.0, e.1) = (e.0 + 1, e.1 + t);
            }
            let mut by: Vec<_> = by.into_iter().collect();
            by.sort_by_key(|(_, (_, t))| Reverse(*t));
            out.extend(note("/optimize caps long descriptions and lists skills you never use by name only."));
            out.push(sub(&format!("{} skills listed, by source", d.skills.len())));
            out.extend(by.into_iter().take(4).map(|(src, (n, t))| row(format!("{src} · {n}"), t)));
            let mut big: Vec<_> = d.skills.iter().collect();
            big.sort_by_key(|s| Reverse(s.2));
            out.push(sub("largest"));
            out.extend(big.into_iter().take(4).map(|(name, _, t)| row(name.clone(), *t)));
        }
        "memory_files" => {
            out.extend(note("/instructions picks which files and sections new sessions load."));
            out.push(sub(&format!("{} files read at the start", d.memory.len())));
            out.extend(d.memory.iter().take(6).map(|(p, ty, t)| row(format!("{} · {}", tilde(p), ty.to_lowercase()), *t)));
        }
        "custom_agents" => {
            out.push(sub(&format!("{} agents listed", d.agents.len())));
            out.extend(d.agents.iter().take(6).map(|(a, src, t)| row(format!("{a} · {src}"), *t)));
        }
        id if id.starts_with("mcp") => {
            let mut by: BTreeMap<&str, (usize, u64, usize)> = BTreeMap::new();
            for (srv, t, loaded) in &d.mcp {
                let e = by.entry(srv).or_default();
                (e.0, e.1, e.2) = (e.0 + 1, e.1 + t, e.2 + usize::from(*loaded));
            }
            let mut by: Vec<_> = by.into_iter().collect();
            by.sort_by_key(|(_, (_, t, _))| Reverse(*t));
            if id == "mcp_instructions" {
                out.extend(note("Instructions the connected MCP servers ask to be sent with every request."));
            } else {
                out.extend(note("A tool's definition is sent only once the model asks for it."));
                out.push(sub("tools by server (loaded of listed)"));
                out.extend(by.into_iter().take(6).map(|(srv, (n, t, l))| row(format!("{srv} · {l}/{n}"), t)));
            }
        }
        "messages" => {
            let m = &d.msg;
            let parts = [("you", m.user), ("Claude", m.assistant), ("tool calls", m.calls), ("tool results", m.results), ("attachments", m.attachments), ("redirected", m.redirected)];
            out.extend(parts.iter().filter(|p| p.1 > 0).map(|(l, t)| row(l.to_string(), *t)));
            let mut tools: Vec<_> = m.tools.iter().collect();
            tools.sort_by_key(|t| Reverse(t.1 + t.2));
            if !tools.is_empty() {
                out.push(sub("tools, calls and results"));
                out.extend(tools.into_iter().take(3).map(|(n, a, b)| row(n.clone(), a + b)));
            }
            let mut kinds: Vec<_> = m.kinds.iter().collect();
            kinds.sort_by_key(|k| Reverse(k.1));
            if !kinds.is_empty() {
                out.push(sub("attachments"));
                out.extend(kinds.into_iter().take(3).map(|(n, t)| row(n.replace('_', " "), *t)));
            }
        }
        "system_prompt" => out.extend(note("Claude Code's built-in instructions, the base of every request.")),
        "system_tools" => out.extend(note("Tool definitions the model can call right away. The rest are deferred and loaded on demand.")),
        "autocompact_buffer" => {
            let at = if d.threshold > 0 { format!(" Auto-compact starts at {}.", tokens(d.threshold)) } else { String::new() };
            out.extend(note(&format!("Held back so there is room to summarise the conversation when auto-compact runs.{at}")));
        }
        "free_space" => {
            let left = d.threshold.saturating_sub(c.total);
            out.extend(note(&match (d.auto, d.threshold) {
                (true, t) if t > 0 => format!("Room left in the window. Auto-compact starts at {}, {} from now.", tokens(t), tokens(left)),
                (false, _) => "Room left in the window. Auto-compact is off, so the conversation stops at the end of it.".to_string(),
                _ => "Room left in the window.".to_string(),
            }));
        }
        _ if cat.kind == Kind::Deferred => out.extend(note("Not in the request until the model asks for it.")),
        _ => {}
    }
    out
}

/// One line under the list on a wide screen.
fn hint(id: &str) -> &'static str {
    match id {
        "skills" => "Every skill's name and description go in every request.",
        "memory_files" => "CLAUDE.md files, loaded at the start of each session.",
        "messages" => "The conversation itself. /compact summarises it when it grows.",
        "mcp_tools_deferred" | "system_tools_deferred" => "On demand: not part of the request yet.",
        "autocompact_buffer" => "Reserved, not used.",
        _ => "",
    }
}

// ---- the panel ----

#[derive(Default, Clone, Copy)]
pub struct Ui {
    pub sel: usize,
}

pub enum Out {
    Keep,
    Close,
    Refresh,
    /// Run the row's [`action`].
    Act,
}

impl Ui {
    /// `n`: how many rows there are.
    pub fn key(&mut self, k: KeyEvent, n: usize) -> Out {
        match k.code {
            KeyCode::Esc => Out::Close,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Out::Close,
            KeyCode::Up | KeyCode::Down if n > 0 => {
                self.sel = cycle(self.sel.min(n - 1), n, k.code == KeyCode::Up);
                Out::Keep
            }
            KeyCode::Home => {
                self.sel = 0;
                Out::Keep
            }
            KeyCode::End => {
                self.sel = n.saturating_sub(1);
                Out::Keep
            }
            KeyCode::Char('r') => Out::Refresh,
            KeyCode::Enter => Out::Act,
            _ => Out::Keep,
        }
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    part as f64 * 100.0 / whole.max(1) as f64
}

/// `width` is the terminal width, which decides whether the detail pane has room.
pub fn view(ui: &Ui, u: &Usage, width: usize) -> View {
    let hints = |act: Option<&str>| {
        let mut h = vec![("↑↓", "move")];
        h.extend(act.map(|a| ("enter", a)));
        h.extend([("r", "refresh"), ("esc", "close")]);
        theme::hints(&h)
    };
    let Some(c) = &u.ctx else {
        let msg = if u.asked { "no context data from this Claude Code" } else { "asking Claude Code…" };
        return View::new("Context", vec![Line::styled(msg, theme::dim())], 0, hints(None));
    };
    let cs = cats(c);
    let sel = ui.sel.min(cs.len().saturating_sub(1));
    let on = cs.get(sel);
    let wide = width >= SIDE_MIN_WIDTH as usize;
    let inner = width.saturating_sub(4);
    let main_w = if wide { inner * 58 / 100 } else { inner };
    let bar_w = main_w.saturating_sub(16).clamp(10, 120);
    let units = bar_w * 8;

    let used_sum: u64 = cs.iter().filter(|k| k.kind == Kind::Used).map(|k| k.tokens).sum();
    let buffer = cs.iter().find(|k| k.kind == Kind::Buffer).map_or(0, |k| k.tokens);
    let pct = percent(c.total, c.max);
    let level = if pct >= 85.0 { theme::danger() } else if pct >= 70.0 { theme::attention() } else { theme::accent() };

    // the summary lines
    let summary = vec![Span::styled(c.model.clone(), theme::bold()), Span::raw("  "), Span::styled(format!("{} of {}", tokens(c.total), tokens(c.max)), theme::bold()), Span::styled(format!(" ({pct:.0}%)"), level)];
    let d = &c.detail;
    let compact = match (d.auto, d.threshold) {
        (true, t) if t > 0 => format!("auto-compact at {} · {} to go", tokens(t), tokens(t.saturating_sub(c.total))),
        (false, _) => "auto-compact off".to_string(),
        _ => String::new(),
    };

    // the gauge: used, free, reserved
    let b_n = share(&[buffer], c.max, units)[0];
    let u_n = share(&[used_sum], c.max, units)[0].min(units - b_n);
    let gauge: Vec<(Fill, usize)> = vec![(Fill::Solid(fg(level)), u_n), (Fill::Free, units - u_n - b_n), (Fill::Reserved, b_n)];
    let mut window = vec![Span::styled(format!("{:<7}", "window"), theme::dim())];
    window.extend(stack(&gauge, bar_w));
    window.push(Span::styled(format!(" {pct:>3.0}%"), level));

    // how the used part divides
    let weights: Vec<u64> = cs.iter().filter(|k| k.kind == Kind::Used).map(|k| k.tokens).collect();
    let mix: Vec<(Fill, usize)> = cs.iter().filter(|k| k.kind == Kind::Used).zip(share(&weights, used_sum, units)).map(|(k, n)| (Fill::Solid(fg(k.style())), n)).collect();
    let mut split = vec![Span::styled(format!("{:<7}", "used"), theme::dim())];
    split.extend(stack(&mix, bar_w));
    split.push(Span::styled(format!(" {:>4}", tokens(used_sum)), theme::dim()));

    let last = match d.last {
        Some([input, out, written, read]) => {
            let all = input + written + read;
            Line::styled(format!("last request  {} new · {} cached ({:.0}% hit) · {} out", tokens(input + written), tokens(read), percent(read, all), tokens(out)), theme::dim())
        }
        None => Line::raw(""),
    };
    let head = vec![Line::from(summary), Line::from(window), Line::from(split), Line::styled(compact, theme::dim()), last];

    // one row per category
    let name_w = main_w.saturating_sub(2 + 2 + 8 + 12 + 6).clamp(12, 32);
    let body: Vec<Line<'static>> = cs
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let at = i == sel;
            let name = |extra: &str| Span::styled(format!("{:<name_w$}", crate::ui::truncate(&format!("{}{extra}", k.name), name_w)), if at { theme::bold() } else { Style::new() });
            let spans = match k.kind {
                Kind::Used => {
                    let share = k.tokens as f64 / used_sum.max(1) as f64;
                    let (full, rest) = crate::motion::look().bar(share, 10);
                    vec![
                        Span::styled("■ ", k.style()),
                        name(""),
                        Span::styled(format!("{:>7} ", tokens(k.tokens)), theme::bold()),
                        Span::styled(full, k.style()),
                        Span::styled(rest, theme::dim()),
                        Span::styled(if share < 0.005 { " <1%".to_string() } else { format!(" {:>3.0}%", share * 100.0) }, theme::dim()),
                    ]
                }
                Kind::Deferred => vec![Span::styled("□ ", theme::dim()), Span::styled(format!("{:<name_w$}", crate::ui::truncate(&format!("{} · on demand", k.name), name_w)), theme::dim()), Span::styled(format!("{:>7}", tokens(k.tokens)), theme::dim())],
                Kind::Buffer => vec![Span::styled("▒ ", theme::dim()), Span::styled(format!("{:<name_w$}", crate::ui::truncate(&format!("{} · reserved", k.name), name_w)), theme::dim()), Span::styled(format!("{:>7}", tokens(k.tokens)), theme::dim())],
                Kind::Free => vec![Span::styled("· ", theme::dim()), name(""), Span::styled(format!("{:>7} ", tokens(k.tokens)), theme::bold()), Span::styled(format!("{:.0}% of the window", percent(k.tokens, c.max)), theme::dim())],
            };
            row_with(at, false, spans)
        })
        .collect();

    let act = action(c, sel).map(|a| a.1);
    let text_w = if wide { (inner * 42 / 100).saturating_sub(3) } else { inner.saturating_sub(2) };
    let lines = on.map(|k| detail(c, k, text_w)).unwrap_or_default();
    let mut v = View::new("Context", body, sel, hints(act)).with_head(head).rows(if wide { cs.len().max(14) } else { cs.len() }, 16);
    if wide {
        let cols = (text_w / 2).clamp(8, 20);
        let mut side = grid(&cs, c.max, on.map_or("", |k| k.id.as_str()), 8, cols);
        side.push(Line::raw(""));
        side.extend(lines.into_iter().take(12));
        v = v.with_side(side).with_tail(vec![Line::raw(""), Line::styled(on.map_or("", |k| hint(&k.id)), theme::dim())]);
    } else {
        let mut tail = vec![Line::raw("")];
        tail.extend(lines.into_iter().take(6));
        tail.resize(7, Line::raw(""));
        v = v.with_tail(tail);
    }
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::panel;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use serde_json::json;

    /// What a session that has used tools reports (trimmed from a real `get_context_usage` reply).
    pub(crate) fn reply() -> Value {
        json!({
            "totalTokens": 20213, "maxTokens": 1000000, "model": "claude-haiku-5-5",
            "categories": [
                {"id": "system_prompt", "name": "System prompt", "tokens": 3278, "kind": "used"},
                {"id": "system_tools", "name": "System tools", "tokens": 8441, "kind": "used"},
                {"id": "mcp_instructions", "name": "MCP instructions", "tokens": 717, "kind": "used"},
                {"id": "mcp_tools_deferred", "name": "MCP tools (deferred)", "tokens": 2234, "kind": "deferred"},
                {"id": "skills", "name": "Skills", "tokens": 2281, "kind": "used"},
                {"id": "messages", "name": "Messages", "tokens": 5496, "kind": "used"},
                {"id": "autocompact_buffer", "name": "Autocompact buffer", "tokens": 33000, "kind": "buffer"},
                {"id": "free_space", "name": "Free space", "tokens": 946787, "kind": "free"}
            ],
            "autoCompactThreshold": 967000, "isAutoCompactEnabled": true,
            "memoryFiles": [{"path": "/x/CLAUDE.md", "type": "User", "tokens": 265}],
            "mcpTools": [{"name": "mcp__a__one", "serverName": "a", "tokens": 205, "isLoaded": false}, {"name": "mcp__a__two", "serverName": "a", "tokens": 500, "isLoaded": true}],
            "agents": [{"agentType": "reviewer", "source": "plugin", "tokens": 71}],
            "skills": {"totalSkills": 3, "tokens": 2281, "skillFrontmatter": [
                {"name": "comfy", "source": "userSettings", "tokens": 64}, {"name": "ship", "source": "plugin", "tokens": 40}, {"name": "init", "source": "built-in", "tokens": 20}]},
            "messageBreakdown": {"toolCallTokens": 77, "toolResultTokens": 51, "attachmentTokens": 5000, "assistantMessageTokens": 270, "userMessageTokens": 16, "redirectedContextTokens": 0,
                "toolCallsByType": [{"name": "Bash", "callTokens": 39, "resultTokens": 26}], "attachmentsByType": [{"name": "prompt_snapshot", "tokens": 4000}]},
            "apiUsage": {"input_tokens": 2, "output_tokens": 62, "cache_creation_input_tokens": 2247, "cache_read_input_tokens": 17963}
        })
    }

    fn usage() -> Usage {
        Usage { ctx: crate::usage::parse_context(&reply()), plan: vec![], asked: true }
    }

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn screen(v: View, w: u16, h: u16) -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| {
            let rows = panel::height(&v, h);
            panel::render(f, Rect::new(0, h - rows, w, rows), v);
        })
        .unwrap();
        let b = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| b[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
    }

    #[test]
    fn the_reply_keeps_what_the_panel_draws() {
        let d = parse(&reply());
        assert_eq!((d.threshold, d.auto, d.memory.len(), d.mcp.len(), d.skills.len()), (967000, true, 1, 2, 3));
        assert_eq!((d.msg.calls, d.msg.tools[0].0.as_str(), d.msg.kinds[0].1), (77, "Bash", 4000));
        assert_eq!(d.last, Some([2, 62, 2247, 17963]));
        assert_eq!(d.ids.len(), 8);
        assert_eq!(parse(&json!({})).last, None, "a fresh session has no last request");
    }

    #[test]
    fn rows_are_largest_first_then_deferred_buffer_and_free() {
        let c = usage().ctx.unwrap();
        let names: Vec<_> = cats(&c).into_iter().map(|k| k.name).collect();
        assert_eq!(names, ["System tools", "Messages", "System prompt", "Skills", "MCP instructions", "MCP tools", "Autocompact buffer", "Free space"]);
        assert_eq!(action(&c, 3), Some(("/optimize", "optimize")));
        assert_eq!(action(&c, 0), None);
    }

    #[test]
    fn shares_add_up_and_nothing_vanishes() {
        for units in [40, 160, 800] {
            let s = share(&[3278, 8441, 717, 2281, 5496], 20213, units);
            assert_eq!(s.iter().sum::<usize>(), units, "a bar of the whole is exactly full");
        }
        let s = share(&[1_000_000, 1], 1_000_001, 80);
        assert_eq!((s[1], s.iter().sum::<usize>()), (1, 80), "a tiny part still gets a unit, even after a huge one");
        let s = share(&[16_000], 1_000_000, 160);
        assert_eq!(s, [3], "against the window it is small");
        assert_eq!(share(&[0, 5], 5, 10), [0, 10]);
    }

    #[test]
    fn a_bar_is_exact_to_an_eighth_and_always_the_width_asked() {
        let red = Color::Red;
        let bar = stack(&[(Fill::Solid(red), 12), (Fill::Free, 28)], 5);
        let glyphs: Vec<&str> = bar.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(glyphs, ["█", "▌", "░", "░", "░"], "12 eighths = one block and a half");
        assert_eq!(bar[1].style.fg, Some(red));
        let two = stack(&[(Fill::Solid(Color::Red), 3), (Fill::Solid(Color::Blue), 5)], 1);
        assert_eq!((two[0].content.as_ref(), two[0].style.fg, two[0].style.bg), ("▍", Some(Color::Red), Some(Color::Blue)), "colour meets colour inside one character");
        let many = stack(&[(Fill::Solid(red), 1), (Fill::Solid(Color::Green), 1), (Fill::Solid(Color::Blue), 1), (Fill::Free, 77)], 10);
        assert_eq!(many.len(), 10, "three slivers in one character still fit it");
        assert_eq!(stack(&[], 4).len(), 4);
    }

    #[test]
    fn the_grid_is_to_scale_but_the_cursor_row_is_always_there() {
        let c = usage().ctx.unwrap();
        let cs = cats(&c);
        let squares = |sel: &str| grid(&cs, c.max, sel, 8, 20).iter().map(text).collect::<String>();
        for sel in ["system_prompt", "skills", "messages"] {
            let n = squares(sel).matches(['■', '█']).count();
            assert!((2..=4).contains(&n), "20k of a 1M window is about three of 160 squares, not one per category (five): {n} with {sel} selected");
        }
        assert!(squares("mcp_instructions").contains('█'), "717 tokens is a fraction of a square, and still shown while it is the cursor row");
        assert!(!squares("skills").contains("█ █ █"), "and the others do not take squares they have not earned");
    }

    #[test]
    fn swatches_and_bar_segments_share_their_colours() {
        let v = view(&Ui::default(), &usage(), 120);
        let swatch = v.body[0].spans[1].style.fg;
        let first_segment = v.head[2].spans[1].style.fg;
        assert!(swatch.is_some() && swatch == first_segment, "{swatch:?} vs {first_segment:?}");
        assert!(v.body[1].spans[1].style.fg.is_some());
        assert_ne!(v.body[0].spans[1].style.fg, v.body[1].spans[1].style.fg, "neighbouring categories differ");
    }

    #[test]
    fn odd_replies_do_not_panic() {
        let weird = |r: Value| {
            let u = Usage { ctx: crate::usage::parse_context(&r), plan: vec![], asked: true };
            for w in [30, 80, 140] {
                for sel in [0, 3, 99] {
                    let v = view(&Ui { sel }, &u, w);
                    assert!(screen(v, w as u16, 30).iter().all(|r| r.chars().count() <= w));
                }
            }
        };
        weird(json!({"categories": [], "totalTokens": 0, "maxTokens": 0}));
        weird(json!({"categories": [{"name": "Free space", "tokens": 5, "kind": "free"}], "totalTokens": 9, "maxTokens": 1}));
        weird(json!({"categories": [{"id": "skills", "name": "Skills", "tokens": 50, "kind": "used"}, {"name": "Autocompact buffer", "tokens": 70, "kind": "buffer"}], "totalTokens": 50, "maxTokens": 100, "autoCompactThreshold": 10}));
        weird(json!({"categories": [{"id": "x", "name": "Something new", "tokens": 7, "kind": "mystery"}], "totalTokens": 7, "maxTokens": 100, "isAutoCompactEnabled": false}));
    }

    #[test]
    fn the_grid_fills_used_then_free_then_the_reserve_and_marks_the_row() {
        let c = usage().ctx.unwrap();
        let cs = cats(&c);
        let g = grid(&cs, c.max, "skills", 8, 20);
        assert_eq!(g.len(), 8);
        assert!(g.iter().all(|l| l.spans.len() == 40), "a square and a space per cell");
        let flat: String = g.iter().map(text).collect();
        assert!(flat.contains('█') && flat.contains('■'), "the cursor row's squares are bigger than the rest");
        assert!(text(&g[7]).ends_with("▒ "), "the reserve is in the last cells");
        assert!(text(&g[0]).contains('·') || text(&g[1]).contains('·'), "free space between");
        let none = grid(&cs, c.max, "free_space", 8, 20);
        assert!(none.iter().map(text).collect::<String>().contains('○'));
    }

    #[test]
    fn a_wide_screen_shows_bars_list_grid_and_what_the_row_is() {
        let u = usage();
        let rows = screen(view(&Ui { sel: 3 }, &u, 120), 120, 40);
        let all = rows.join("\n");
        assert!(all.contains("claude-haiku-5-5") && all.contains("20.2k of 1.0M") && all.contains("auto-compact at 967.0k · 946.8k to go"), "{all}");
        assert!(all.contains("window") && all.contains("used") && all.contains("last request") && all.contains("89% hit"), "{all}");
        assert!(all.contains("System tools") && all.contains("MCP tools · on demand") && all.contains("Autocompact buffer · reserved") && all.contains("95% of the window"), "{all}");
        assert!(all.contains("3 skills listed, by source") && !all.contains("claude.ai") && all.contains("yours · 1"), "the cursor row is Skills: {all}");
        assert!(all.contains("/optimize caps") && all.contains("⏎ optimize"), "{all}");
        assert!(all.contains("■") && all.contains("█"), "the grid is drawn");
        let other = screen(view(&Ui { sel: 1 }, &u, 120), 120, 40).join("\n");
        assert!(other.contains("tool calls") && other.contains("prompt snapshot") && other.contains("Bash"), "messages: {other}");
        assert!(!other.contains("⏎"), "no action on this row");
    }

    #[test]
    fn a_narrow_screen_keeps_the_bars_and_puts_a_short_detail_under_the_list() {
        let u = usage();
        let a = screen(view(&Ui { sel: 3 }, &u, 70), 70, 40);
        let all = a.join("\n");
        assert!(all.contains("window") && all.contains("System tools"), "{all}");
        assert!(all.contains("3 skills listed"), "the detail moved under the list: {all}");
        assert!(!all.contains("■ ■ ■"), "no grid");
        assert!(a.iter().all(|r| r.chars().count() <= 70));
        let heights: Vec<usize> = (0..8).map(|i| view(&Ui { sel: i }, &u, 70).tail.len()).collect();
        assert!(heights.windows(2).all(|p| p[0] == p[1]), "the panel does not change height as the cursor moves: {heights:?}");
        // and nothing is wider than the screen at any size, on any row
        for w in [40u16, 60, 90, 130] {
            for i in 0..8 {
                assert!(screen(view(&Ui { sel: i }, &u, w as usize), w, 36).iter().all(|r| r.chars().count() <= w as usize), "width {w} row {i}");
            }
        }
    }

    #[test]
    fn before_the_reply_arrives_it_says_so_and_keys_move_and_close() {
        let mut u = Usage::default();
        assert!(text(&view(&Ui::default(), &u, 100).body[0]).contains("asking Claude Code"));
        u.asked = true;
        assert!(text(&view(&Ui::default(), &u, 100).body[0]).contains("no context data"));
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let mut ui = Ui::default();
        assert!(matches!(ui.key(key(KeyCode::Up), 8), Out::Keep) && ui.sel == 7, "wraps");
        assert!(matches!(ui.key(key(KeyCode::Down), 8), Out::Keep) && ui.sel == 0);
        assert!(matches!(ui.key(key(KeyCode::End), 8), Out::Keep) && ui.sel == 7);
        assert!(matches!(ui.key(key(KeyCode::Char('r')), 8), Out::Refresh));
        assert!(matches!(ui.key(key(KeyCode::Enter), 8), Out::Act));
        assert!(matches!(ui.key(key(KeyCode::Esc), 8), Out::Close));
        assert!(matches!(ui.key(key(KeyCode::Up), 0), Out::Keep), "no rows, no panic");
    }
}
