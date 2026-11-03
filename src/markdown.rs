//! Markdown → styled terminal lines. Covers what model output actually uses: headings, paragraphs
//! with **bold** / *italic* / ~~strike~~ / `code` / links, bullet, numbered and task lists, block
//! quotes, fenced code (diff-aware), tables and rules. Unmatched markers stay literal, so text that
//! is still streaming in never flickers into odd styling. Every returned line fits `width`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Seg = (String, Style);


fn dim() -> Style {
    crate::theme::dim()
}

fn segs_width(s: &[Seg]) -> usize {
    s.iter().map(|(t, _)| t.width()).sum()
}

pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(4);
    let lines: Vec<&str> = text.trim_matches('\n').split('\n').collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    // Indent (in columns) of the text of the list item just rendered, for indented continuation lines.
    let mut item_indent: Option<usize> = None;
    let mut i = 0;
    while i < lines.len() {
        let src = lines[i].trim_end();
        let t = src.trim_start();
        let lead = src.len() - t.len();
        if t.starts_with("```") || t.starts_with("~~~") {
            let fence = &t[..3];
            let lang = t.trim_start_matches(['`', '~']).trim().to_string();
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
                body.push(lines[i]);
                i += 1;
            }
            i += 1; // closing fence (or end of a block that is still streaming)
            code_block(&mut out, &lang, &body, width);
            item_indent = None;
        } else if i + 1 < lines.len() && t.contains('|') && is_separator(lines[i + 1]) {
            let aligns: Vec<char> = split_cells(lines[i + 1])
                .iter()
                .map(|c| match (c.trim().starts_with(':'), c.trim().ends_with(':')) {
                    (true, true) => 'c',
                    (_, true) => 'r',
                    _ => 'l',
                })
                .collect();
            let mut rows = vec![src];
            i += 2;
            while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
                rows.push(lines[i].trim_end());
                i += 1;
            }
            table(&mut out, &rows, &aligns, width);
            item_indent = None;
        } else if let Some(level) = heading_level(t) {
            let style = match level {
                1 => Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                2 => Style::new().add_modifier(Modifier::BOLD),
                _ => Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC),
            };
            let body = t.trim_start_matches('#').trim();
            emit(&mut out, wrap_segs(&inline(body, style), width), Span::raw(""), Span::raw(""));
            i += 1;
            item_indent = None;
        } else if is_rule(t) {
            out.push(Line::styled("─".repeat(width.min(40)), dim()));
            i += 1;
            item_indent = None;
        } else if t.starts_with('>') {
            let mut inner = Vec::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                inner.push(lines[i].trim_start().trim_start_matches('>').strip_prefix(' ').unwrap_or(lines[i].trim_start().trim_start_matches('>')));
                i += 1;
            }
            for mut l in render(&inner.join("\n"), width.saturating_sub(2)) {
                l.spans.insert(0, Span::styled("▎ ", dim()));
                out.push(l);
            }
            item_indent = None;
        } else if let Some((marker, body)) = list_item(t) {
            let depth = (lead / 2).min(4);
            let (bullet, task) = match task_box(body) {
                Some((done, rest)) => (if done { "☑ " } else { "☐ " }.to_string(), Some(rest)),
                None if marker == "•" => (["• ", "◦ ", "▪ ", "· ", "· "][depth].to_string(), None),
                None => (format!("{marker} "), None),
            };
            let pad = "  ".repeat(depth);
            let first = format!("{pad}{bullet}");
            let hang = " ".repeat(first.width());
            item_indent = Some(first.width());
            let segs = inline(task.unwrap_or(body), Style::new());
            emit(&mut out, wrap_segs(&segs, width.saturating_sub(first.width())), Span::styled(first, dim()), Span::raw(hang));
            i += 1;
        } else if t.is_empty() {
            if out.last().is_some_and(|l| !l.spans.is_empty()) {
                out.push(Line::raw(""));
            }
            i += 1;
            item_indent = None;
        } else {
            // A line indented under a list item continues it; anything else is a plain paragraph line.
            let indent = if lead >= 2 { item_indent.unwrap_or(0) } else { 0 };
            if indent == 0 {
                item_indent = None;
            }
            let pad = " ".repeat(indent);
            emit(&mut out, wrap_segs(&inline(t, Style::new()), width.saturating_sub(indent)), Span::raw(pad.clone()), Span::raw(pad));
            i += 1;
        }
    }
    while out.last().is_some_and(|l| l.spans.is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        out.push(Line::raw(""));
    }
    out
}

fn emit(out: &mut Vec<Line<'static>>, lines: Vec<Vec<Seg>>, first: Span<'static>, rest: Span<'static>) {
    for (i, l) in lines.into_iter().enumerate() {
        let mut spans = vec![if i == 0 { first.clone() } else { rest.clone() }];
        spans.extend(l.into_iter().map(|(t, s)| Span::styled(t, s)));
        out.push(Line::from(spans));
    }
}

fn heading_level(t: &str) -> Option<usize> {
    let n = t.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&n).then_some(n).filter(|n| t[*n..].is_empty() || t[*n..].starts_with(' '))
}

fn is_rule(t: &str) -> bool {
    let c: Vec<char> = t.chars().filter(|c| !c.is_whitespace()).collect();
    c.len() >= 3 && matches!(c[0], '-' | '*' | '_') && c.iter().all(|x| *x == c[0])
}

/// ("•" or "1.", rest of line) for a bullet or numbered item.
fn list_item(t: &str) -> Option<(String, &str)> {
    let mut chars = t.chars();
    let c = chars.next()?;
    if matches!(c, '-' | '*' | '+') && t[1..].starts_with(' ') && !is_rule(t) {
        return Some(("•".into(), t[1..].trim_start()));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) && matches!(t[digits..].chars().next(), Some('.' | ')')) && t[digits + 1..].starts_with(' ') {
        return Some((format!("{}.", &t[..digits]), t[digits + 1..].trim_start()));
    }
    None
}

fn task_box(body: &str) -> Option<(bool, &str)> {
    let rest = body.get(3..)?;
    match body.get(..3)? {
        "[ ]" if rest.starts_with(' ') || rest.is_empty() => Some((false, rest.trim_start())),
        "[x]" | "[X]" if rest.starts_with(' ') || rest.is_empty() => Some((true, rest.trim_start())),
        _ => None,
    }
}

fn code_block(out: &mut Vec<Line<'static>>, lang: &str, body: &[&str], width: usize) {
    if !lang.is_empty() {
        out.push(Line::styled(format!("╭ {lang}"), dim()));
    }
    let gutter = || Span::styled("│ ", dim());
    for src in body {
        let style = if lang == "diff" {
            match src.chars().next() {
                Some('+') => crate::theme::ok(),
                Some('-') => crate::theme::danger(),
                Some('@') => crate::theme::accent(),
                _ => crate::theme::code(),
            }
        } else if lang.is_empty() {
            crate::theme::code()
        } else {
            // Highlighted, then broken at the width piece by piece so the colours survive the wrap.
            let mut row: Vec<Span<'static>> = vec![gutter()];
            let mut used = 0;
            for (tok, st) in highlight(&src.replace('\t', "    "), lang) {
                for ch in tok.chars() {
                    let w = ch.width().unwrap_or(0);
                    if used + w > width.saturating_sub(2).max(1) {
                        out.push(Line::from(std::mem::replace(&mut row, vec![gutter()])));
                        used = 0;
                    }
                    let joins = row.len() > 1 && row.last().is_some_and(|l| l.style == st);
                    match row.last_mut() {
                        Some(last) if joins => last.content.to_mut().push(ch),
                        _ => row.push(Span::styled(ch.to_string(), st)),
                    }
                    used += w;
                }
            }
            out.push(Line::from(row));
            continue;
        };
        for part in hard_wrap(&src.replace('\t', "    "), width.saturating_sub(2)) {
            out.push(Line::from(vec![gutter(), Span::styled(part, style)]));
        }
    }
}

const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "use", "mod", "impl", "struct", "enum", "trait", "match", "if", "else", "for", "while", "loop",
    "return", "break", "continue", "in", "as", "where", "self", "Self", "crate", "super", "const", "static", "async", "await",
    "move", "ref", "type", "dyn", "unsafe", "extern", "true", "false", "None", "Some", "Ok", "Err", "def", "class", "import",
    "from", "lambda", "pass", "yield", "with", "try", "except", "finally", "raise", "not", "and", "or", "is", "elif", "global",
    "True", "False", "function", "var", "new", "this", "typeof", "instanceof", "export", "default", "interface", "extends",
    "implements", "package", "func", "go", "defer", "chan", "range", "switch", "case", "void", "int", "char", "bool", "long",
    "double", "float", "public", "private", "protected", "throw", "throws", "catch", "null", "nil", "end", "do", "then", "fi",
    "esac", "done", "local", "export", "select", "insert", "update", "delete", "create", "table", "values", "set", "join",
    "begin", "end", "elseif", "undefined", "of", "readonly", "override", "virtual", "namespace", "using", "template", "auto",
];

/// One line of code in `lang` as (text, style) runs. A light lexer, not a parser: comments to the end of the
/// line, quoted strings, numbers, and one shared keyword list. Good enough to make code read like code.
fn highlight(line: &str, lang: &str) -> Vec<(String, Style)> {
    let lang = lang.to_ascii_lowercase();
    let hash = ["python", "py", "sh", "bash", "zsh", "shell", "console", "ruby", "rb", "yaml", "yml", "toml", "perl", "r", "make", "makefile", "dockerfile", "ini", "conf", "nix", "elixir", "ex"];
    let comment = if hash.contains(&lang.as_str()) {
        "#"
    } else if ["sql", "lua", "haskell", "hs"].contains(&lang.as_str()) {
        "--"
    } else {
        "//"
    };
    let ci = lang == "sql";
    let code = Style::new();
    let ch: Vec<char> = line.chars().collect();
    let at = |i: usize, pat: &str| pat.chars().enumerate().all(|(k, p)| ch.get(i + k) == Some(&p));
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut i = 0;
    while i < ch.len() {
        if at(i, comment) || (comment == "//" && at(i, "/*")) {
            out.push((ch[i..].iter().collect(), crate::theme::syn_comment()));
            break;
        }
        let c = ch[i];
        // A Rust lifetime ('a) is not a char literal: only 'x' and '\n' are.
        let quote = matches!(c, '"' | '`') || (c == '\'' && (lang != "rust" && lang != "rs" || ch.get(i + 2) == Some(&'\'') || ch.get(i + 1) == Some(&'\\')));
        let start = i;
        let style = if quote {
            i += 1;
            while i < ch.len() && ch[i] != c {
                i += if ch[i] == '\\' { 2 } else { 1 };
            }
            i = (i + 1).min(ch.len());
            crate::theme::syn_string()
        } else if c.is_ascii_digit() {
            while i < ch.len() && (ch[i].is_ascii_alphanumeric() || ch[i] == '_' || ch[i] == '.') {
                i += 1;
            }
            crate::theme::syn_number()
        } else if c.is_alphabetic() || c == '_' {
            while i < ch.len() && (ch[i].is_alphanumeric() || ch[i] == '_') {
                i += 1;
            }
            let w: String = ch[start..i].iter().collect();
            let w = if ci { w.to_ascii_lowercase() } else { w };
            if KEYWORDS.contains(&w.as_str()) { crate::theme::syn_keyword() } else { code }
        } else {
            i += 1;
            code
        };
        let tok: String = ch[start..i].iter().collect();
        match out.last_mut() {
            Some((t, st)) if *st == style => t.push_str(&tok),
            _ => out.push((tok, style)),
        }
    }
    out
}

/// Break at exactly `width` columns, ignoring word boundaries (code, long tokens).
fn hard_wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![String::new()];
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > width {
            out.push(String::new());
            w = 0;
        }
        out.last_mut().unwrap().push(c);
        w += cw;
    }
    out
}

// ---------- tables ----------

fn is_separator(line: &str) -> bool {
    let cells = split_cells(line);
    !cells.is_empty()
        && line.contains('-')
        && cells.iter().all(|c| {
            let c = c.trim();
            !c.is_empty() && c.trim_matches(':').chars().all(|x| x == '-') && c.contains('-')
        })
}

fn split_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').filter(|_| !t.ends_with("\\|")).unwrap_or(t);
    let mut cells = vec![String::new()];
    let mut esc = false;
    for c in t.chars() {
        match (c, esc) {
            ('|', false) => cells.push(String::new()),
            ('\\', false) => esc = true,
            _ => {
                if esc && c != '|' {
                    cells.last_mut().unwrap().push('\\');
                }
                cells.last_mut().unwrap().push(c);
                esc = false;
            }
        }
    }
    cells
}

fn truncate_segs(segs: Vec<Seg>, width: usize) -> Vec<Seg> {
    if segs_width(&segs) <= width {
        return segs;
    }
    let mut out = Vec::new();
    let mut w = 0;
    'outer: for (t, s) in segs {
        let mut buf = String::new();
        for c in t.chars() {
            let cw = c.width().unwrap_or(0);
            if w + cw >= width {
                out.push((buf, s));
                break 'outer;
            }
            buf.push(c);
            w += cw;
        }
        out.push((buf, s));
    }
    out.push(("…".into(), dim()));
    out
}

/// `rows[0]` is the header; the separator line is not included. `aligns` holds l/c/r per column.
fn table(out: &mut Vec<Line<'static>>, rows: &[&str], aligns: &[char], width: usize) {
    let header = split_cells(rows[0]);
    let n = header.len();
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut grid: Vec<Vec<Vec<Seg>>> = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let mut cells = split_cells(row);
        cells.resize(n, String::new());
        grid.push(cells.iter().map(|c| inline(c.trim(), if r == 0 { bold } else { Style::new() })).collect());
    }
    let mut w: Vec<usize> = (0..n).map(|c| grid.iter().map(|r| segs_width(&r[c])).max().unwrap_or(0).max(1)).collect();
    let overhead = 3 * n + 1;
    // Narrow the widest column until the table fits; give up below 4 columns each.
    while w.iter().sum::<usize>() + overhead > width {
        let (idx, &widest) = w.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
        if widest <= 4 {
            for row in rows {
                out.extend(wrap_segs(&inline(row.trim(), Style::new()), width).into_iter().map(|l| {
                    Line::from(l.into_iter().map(|(t, s)| Span::styled(t, s)).collect::<Vec<_>>())
                }));
            }
            return;
        }
        w[idx] -= 1;
    }
    let rule = |l: &str, m: &str, r: &str| {
        let body = w.iter().map(|x| "─".repeat(x + 2)).collect::<Vec<_>>().join(m);
        Line::styled(format!("{l}{body}{r}"), dim())
    };
    out.push(rule("┌", "┬", "┐"));
    for (r, row) in grid.into_iter().enumerate() {
        let mut spans = vec![Span::styled("│", dim())];
        for (c, cell) in row.into_iter().enumerate() {
            let cell = truncate_segs(cell, w[c]);
            let pad = w[c].saturating_sub(segs_width(&cell));
            let left = match aligns.get(c) {
                Some('r') => pad,
                Some('c') => pad / 2,
                _ => 0,
            };
            spans.push(Span::raw(" ".repeat(left + 1)));
            spans.extend(cell.into_iter().map(|(t, s)| Span::styled(t, s)));
            spans.push(Span::raw(" ".repeat(pad - left + 1)));
            spans.push(Span::styled("│", dim()));
        }
        out.push(Line::from(spans));
        if r == 0 {
            out.push(rule("├", "┼", "┤"));
        }
    }
    out.push(rule("└", "┴", "┘"));
}

// ---------- inline ----------

fn inline(s: &str, base: Style) -> Vec<Seg> {
    inline_with(s, base, false)
}

/// `keep`: the markers stay in the text, dimmed, so the segments join back into `s` exactly (the input box).
fn inline_with(s: &str, base: Style, keep: bool) -> Vec<Seg> {
    let ch: Vec<char> = s.chars().collect();
    let n = ch.len();
    let mut out: Vec<Seg> = Vec::new();
    let mut buf = String::new();
    let (mut bold, mut ital, mut strike) = (false, false, false);
    let mut open: Vec<(char, usize)> = Vec::new();
    // Markers with no closer anywhere after some point have none after any later point either:
    // remembering that keeps a long line of unmatched `*` linear instead of quadratic.
    // Only without backticks: `has_closer` reads code spans a little differently from this loop, so with
    // them a later start can see a closer an earlier one did not.
    let mut no_closer: Vec<(char, usize)> = Vec::new();
    let memo = !ch.contains(&'`');
    let style = |bold: bool, ital: bool, strike: bool| {
        let mut st = base;
        if bold {
            st = st.add_modifier(Modifier::BOLD);
        }
        if ital {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if strike {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        st
    };
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                out.push((std::mem::take(&mut buf), style(bold, ital, strike)));
            }
        };
    }
    let run = |i: usize, c: char| ch[i..].iter().take_while(|x| **x == c).count();
    let link = crate::theme::accent().add_modifier(Modifier::UNDERLINED);
    let mut i = 0;
    while i < n {
        let c = ch[i];
        match c {
            '\\' if i + 1 < n && ch[i + 1].is_ascii_punctuation() => {
                if keep {
                    buf.push('\\');
                }
                buf.push(ch[i + 1]);
                i += 2;
            }
            '`' => {
                let k = run(i, '`');
                let close = (i + k..n).find(|&j| ch[j] == '`' && run(j, '`') == k && (j == 0 || ch[j - 1] != '`'));
                match close {
                    Some(j) => {
                        flush!();
                        let code: String = ch[i + k..j].iter().collect();
                        if keep {
                            let ticks = "`".repeat(k);
                            out.extend([(ticks.clone(), dim()), (code, base.patch(crate::theme::code())), (ticks, dim())]);
                            i = j + k;
                            continue;
                        }
                        let code = if code.starts_with(' ') && code.ends_with(' ') && code.trim().len() + 2 <= code.len() { code[1..code.len() - 1].to_string() } else { code };
                        out.push((code, base.patch(crate::theme::code())));
                        i = j + k;
                    }
                    None => {
                        buf.extend(std::iter::repeat_n('`', k));
                        i += k;
                    }
                }
            }
            '*' | '_' | '~' => {
                let k = run(i, c);
                let prev_ws = i == 0 || ch[i - 1].is_whitespace();
                let next_ws = i + k >= n || ch[i + k].is_whitespace();
                let prev_word = i > 0 && ch[i - 1].is_alphanumeric();
                let next_word = i + k < n && ch[i + k].is_alphanumeric();
                let valid = match c {
                    '~' => k == 2,
                    _ => (1..=3).contains(&k),
                };
                let intra = c == '_'; // snake_case_names are not emphasis
                let can_close = valid && !prev_ws && !(intra && next_word);
                let can_open = valid && !next_ws && !(intra && prev_word);
                if valid && can_close && open.contains(&(c, k)) {
                    flush!();
                    open.retain(|o| *o != (c, k));
                    if keep {
                        out.push((ch[i..i + k].iter().collect(), dim()));
                    }
                    toggle(c, k, &mut bold, &mut ital, &mut strike);
                    i += k;
                } else if valid && can_open && !no_closer.contains(&(c, k)) && (has_closer(&ch, i + k, c, k) || {
                    if memo {
                        no_closer.push((c, k));
                    }
                    false
                }) {
                    flush!();
                    open.push((c, k));
                    toggle(c, k, &mut bold, &mut ital, &mut strike);
                    if keep {
                        out.push((ch[i..i + k].iter().collect(), dim()));
                    }
                    i += k;
                } else {
                    buf.extend(std::iter::repeat_n(c, k));
                    i += k;
                }
            }
            '[' | '!' if c == '[' || (i + 1 < n && ch[i + 1] == '[') => {
                let img = c == '!';
                let start = if img { i + 1 } else { i };
                if let Some((text, url, end)) = parse_link(&ch, start) {
                    flush!();
                    if keep {
                        out.push((if img { "![" } else { "[" }.into(), dim()));
                        out.extend(inline_with(&text, link, true));
                        out.push((format!("]({url})"), dim()));
                    } else if img {
                        out.push((format!("[image: {text}]"), dim()));
                    } else {
                        out.extend(inline(&text, link));
                        if url != text && !url.starts_with('#') {
                            out.push((format!(" ({url})"), dim()));
                        }
                    }
                    i = end;
                } else {
                    buf.push(c);
                    i += 1;
                }
            }
            'h' if (ch[i..].starts_with(&['h', 't', 't', 'p']) && (i == 0 || ch[i - 1].is_whitespace() || "(<".contains(ch[i - 1]))) => {
                let rest: String = ch[i..].iter().take_while(|c| !c.is_whitespace()).collect();
                if rest.starts_with("http://") || rest.starts_with("https://") {
                    let end = rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len());
                    let url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
                    flush!();
                    out.push((url.to_string(), link));
                    i += url.chars().count();
                } else {
                    buf.push(c);
                    i += 1;
                }
            }
            _ => {
                buf.push(c);
                i += 1;
            }
        }
    }
    flush!();
    out
}

fn toggle(c: char, k: usize, bold: &mut bool, ital: &mut bool, strike: &mut bool) {
    match (c, k) {
        ('~', _) => *strike = !*strike,
        (_, 1) => *ital = !*ital,
        (_, 2) => *bold = !*bold,
        _ => {
            *bold = !*bold;
            *ital = !*ital;
        }
    }
}

/// Is there a later run of exactly `k` `c`s that could close the span (not preceded by a space)?
fn has_closer(ch: &[char], from: usize, c: char, k: usize) -> bool {
    let mut j = from;
    while j < ch.len() {
        if ch[j] == '`' {
            // Skip over code spans: markers inside them do not count.
            let r = ch[j..].iter().take_while(|x| **x == '`').count();
            match (j + r..ch.len()).find(|&e| ch[e] == '`' && ch[e..].iter().take_while(|x| **x == '`').count() == r) {
                Some(e) => j = e + r,
                None => j += r,
            }
            continue;
        }
        if ch[j] == c {
            let r = ch[j..].iter().take_while(|x| **x == c).count();
            let after_word = j + r < ch.len() && ch[j + r].is_alphanumeric();
            if r == k && j > from && !ch[j - 1].is_whitespace() && !(c == '_' && after_word) {
                return true;
            }
            j += r;
        } else {
            j += 1;
        }
    }
    false
}

/// `[text](url)` starting at `ch[start] == '['` → (text, url, index after `)`).
fn parse_link(ch: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut depth = 0;
    let mut j = start;
    while j < ch.len() {
        match ch[j] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        j += 1;
    }
    if j + 1 >= ch.len() || ch[j] != ']' || ch[j + 1] != '(' {
        return None;
    }
    let close = (j + 2..ch.len()).find(|&e| ch[e] == ')')?;
    let url: String = ch[j + 2..close].iter().collect();
    if url.contains(char::is_whitespace) {
        return None;
    }
    Some((ch[start + 1..j].iter().collect(), url, close + 1))
}

/// The input box's text with markdown styling but every character kept: one style per char of `text`
/// (newlines included), so the hard-wrapped, cursor-exact input layout can be painted from it.
pub fn source_styles(text: &str) -> Vec<Style> {
    let mut out = Vec::with_capacity(text.len());
    let mut fence: Option<&str> = None;
    let seg = |segs: Vec<Seg>, out: &mut Vec<Style>| out.extend(segs.into_iter().flat_map(|(t, st)| std::iter::repeat_n(st, t.chars().count())));
    for (i, src) in text.split('\n').enumerate() {
        if i > 0 {
            out.push(Style::new());
        }
        let t = src.trim_start();
        let lead = &src[..src.len() - t.len()];
        let is_fence = t.starts_with("```") || t.starts_with("~~~");
        let segs: Vec<Seg> = match fence {
            Some(f) => {
                if t.starts_with(f) {
                    fence = None;
                }
                vec![(src.into(), if fence.is_none() { dim() } else { crate::theme::code() })]
            }
            None if is_fence => {
                fence = Some(&t[..3]);
                vec![(src.into(), dim())]
            }
            None => {
                let mut v = vec![(lead.to_string(), Style::new())];
                if let Some(n) = heading_level(t) {
                    v.push((t[..n].into(), dim()));
                    v.extend(inline_with(&t[n..], crate::theme::bold(), true));
                } else if let Some(q) = t.strip_prefix('>') {
                    v.push((">".into(), dim()));
                    v.extend(inline_with(q, Style::new(), true));
                } else if let Some((_, body)) = list_item(t) {
                    let m = t.len() - body.len();
                    v.push((t[..m].into(), crate::theme::accent()));
                    v.extend(inline_with(body, Style::new(), true));
                } else {
                    v.extend(inline_with(t, Style::new(), true));
                }
                v
            }
        };
        seg(segs, &mut out);
    }
    out
}

// ---------- wrapping ----------

/// Greedy word wrap that keeps each segment's style. Words are only broken at spaces; one wider than
/// the line is split by column.
fn wrap_segs(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let width = width.max(1);
    let mut words: Vec<Vec<Seg>> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    for (t, s) in segs {
        let mut buf = String::new();
        for c in t.chars() {
            if c == ' ' {
                if !buf.is_empty() {
                    cur.push((std::mem::take(&mut buf), *s));
                }
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            } else {
                buf.push(c);
            }
        }
        if !buf.is_empty() {
            cur.push((buf, *s));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut lines: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut w = 0;
    for word in words {
        let ww = segs_width(&word);
        if w > 0 && w + 1 + ww > width {
            lines.push(Vec::new());
            w = 0;
        }
        if ww > width {
            for (t, s) in word {
                for c in t.chars() {
                    let cw = c.width().unwrap_or(0);
                    if w + cw > width {
                        lines.push(Vec::new());
                        w = 0;
                    }
                    lines.last_mut().unwrap().push((c.to_string(), s));
                    w += cw;
                }
            }
        } else {
            if w > 0 {
                lines.last_mut().unwrap().push((" ".into(), Style::new()));
                w += 1;
            }
            lines.last_mut().unwrap().extend(word);
            w += ww;
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }
    fn text(md: &str, w: usize) -> Vec<String> {
        plain(&render(md, w))
    }
    fn has(md: &str, needle: &str, f: impl Fn(&Style) -> bool) -> bool {
        render(md, 80).iter().flat_map(|l| &l.spans).any(|s| s.content.contains(needle) && f(&s.style))
    }

    #[test]
    fn source_styles_keep_every_char_and_style_the_markup() {
        let md = "# Title\nsome **bold** and `co*de` [a](http://x) \\*no*\n- item *it*\n```rs\nlet *x* = 1;\n```\nsnake_case_name ~~gone~~";
        let st = source_styles(md);
        assert_eq!(st.len(), md.chars().count(), "one style per char, markers and newlines included");
        let at = |needle: &str| st[md[..md.find(needle).unwrap()].chars().count()];
        assert!(at("bold**").add_modifier.contains(Modifier::BOLD));
        assert_eq!(at("**bold"), dim(), "the markers stay, dimmed");
        assert_eq!(at("co*de"), Style::new().patch(crate::theme::code()));
        assert!(at("it*").add_modifier.contains(Modifier::ITALIC));
        assert!(!at("no*").add_modifier.contains(Modifier::ITALIC), "escaped markers are literal");
        assert_eq!(at("let *x*"), crate::theme::code(), "fenced code is not emphasis");
        assert!(at("Title").add_modifier.contains(Modifier::BOLD));
        assert!(!at("case_name").add_modifier.contains(Modifier::ITALIC));
        assert!(at("gone").add_modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(source_styles("**unclosed").len(), 10);
    }

    #[test]
    fn inline_styles_apply_and_markers_vanish() {
        assert_eq!(text("a **b** *c* ~~d~~ `e`", 80), ["a b c d e"]);
        assert!(has("a **b** c", "b", |s| s.add_modifier.contains(Modifier::BOLD)));
        assert!(has("a *b* c", "b", |s| s.add_modifier.contains(Modifier::ITALIC)));
        assert!(has("a ~~b~~ c", "b", |s| s.add_modifier.contains(Modifier::CROSSED_OUT)));
        assert!(has("a `b` c", "b", |s| s.fg == crate::theme::code().fg));
        assert!(has("***both***", "both", |s| s.add_modifier.contains(Modifier::BOLD | Modifier::ITALIC)));
        assert!(!has("plain", "plain", |s| s.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn unmatched_or_intraword_markers_stay_literal() {
        assert_eq!(text("2 * 3 * 4 = 24", 80), ["2 * 3 * 4 = 24"]);
        assert_eq!(text("use snake_case_names and **half-open", 80), ["use snake_case_names and **half-open"]);
        assert_eq!(text("a * b", 80), ["a * b"]);
        assert_eq!(text("price is $5 ~ $10", 80), ["price is $5 ~ $10"]);
        assert_eq!(text(r"escaped \*stars\*", 80), ["escaped *stars*"]);
    }

    #[test]
    fn markers_inside_code_spans_do_not_open_emphasis() {
        assert_eq!(text("`a*b` and *real*", 80), ["a*b and real"]);
        assert!(has("`a*b` and *real*", "real", |s| s.add_modifier.contains(Modifier::ITALIC)));
        assert!(!has("`a*b` and *real*", "a*b", |s| s.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn links_show_text_then_url_and_bare_urls_are_styled() {
        assert_eq!(text("see [the docs](https://x.dev/a) now", 80), ["see the docs (https://x.dev/a) now"]);
        assert_eq!(text("[https://x.dev](https://x.dev)", 80), ["https://x.dev"]);
        assert_eq!(text("go to https://x.dev/a, ok", 80), ["go to https://x.dev/a, ok"]);
        assert!(has("go to https://x.dev/a, ok", "https://x.dev/a", |s| s.add_modifier.contains(Modifier::UNDERLINED)));
        assert_eq!(text("![chart](a.png)", 80), ["[image: chart]"]);
        assert_eq!(text("[not a link] (really)", 80), ["[not a link] (really)"]);
    }

    #[test]
    fn lists_nest_number_and_hang_their_wrapped_lines() {
        assert_eq!(text("- one\n  - two\n    - three", 80), ["• one", "  ◦ two", "    ▪ three"]);
        assert_eq!(text("1. first\n2) second", 80), ["1. first", "2. second"]);
        assert_eq!(text("- [ ] todo\n- [x] done", 80), ["☐ todo", "☑ done"]);
        assert_eq!(text("- alpha beta gamma delta", 14), ["• alpha beta", "  gamma delta"]);
        assert_eq!(text("- item\n  continued here", 80), ["• item", "  continued here"]);
        assert_eq!(text("- a\n\npara", 80), ["• a", "", "para"]);
    }

    #[test]
    fn headings_rules_and_quotes() {
        assert_eq!(text("# Title\n\n## Sub\ntext", 80), ["Title", "", "Sub", "text"]);
        assert!(has("# Title", "Title", |s| s.add_modifier.contains(Modifier::BOLD)));
        assert_eq!(text("#hashtag", 80), ["#hashtag"]);
        assert_eq!(text("a\n\n---\n\nb", 80), ["a", "", &"─".repeat(40), "", "b"]);
        assert_eq!(text("> quoted *text*\n> more", 80), ["▎ quoted text", "▎ more"]);
        assert_eq!(text("- a\n- b\n***", 80)[2], "─".repeat(40));
    }

    #[test]
    fn fenced_code_is_verbatim_labelled_and_survives_streaming() {
        assert_eq!(text("```rust\nlet a = *b* * 2;\n```", 80), ["╭ rust", "│ let a = *b* * 2;"]);
        assert_eq!(text("```\nx\n", 80), ["│ x"], "an unclosed fence (still streaming) renders as code");
        assert_eq!(text("```\nabcdefghij\n```", 8), ["│ abcdef", "│ ghij"]);
        let diff = render("```diff\n+new\n-old\n```", 80);
        assert_eq!(diff[1].spans[1].style, crate::theme::ok());
        assert_eq!(diff[2].spans[1].style, crate::theme::danger());
    }

    #[test]
    fn code_with_a_language_is_highlighted_and_still_wraps() {
        use crate::theme::{syn_comment, syn_keyword, syn_number, syn_string};
        let runs = highlight;
        let py = runs("def f(x): return 'a#b' + 2  # note", "python");
        assert!(py.contains(&("def".into(), syn_keyword())) && py.contains(&("'a#b'".into(), syn_string())));
        assert!(py.contains(&("2".into(), syn_number())) && py.contains(&("# note".into(), syn_comment())), "{py:?}");
        let rs = runs("fn f<'a>(s: &'a str) -> char { 'x' } // end", "rust");
        assert!(rs.contains(&("'x'".into(), syn_string())) && !rs.iter().any(|(t, s)| t.contains("'a>") && *s == syn_string()), "a lifetime is not a string: {rs:?}");
        assert_eq!(runs("SELECT 1", "sql")[0], ("SELECT".into(), syn_keyword()));
        assert_eq!(text("```python\nreturn x\n```", 6), ["╭ python", "│ retu", "│ rn x"], "wraps at the width");
    }

    #[test]
    fn tables_align_and_shrink_to_fit() {
        let md = "| Name | Qty |\n|---|---:|\n| apple | 3 |\n| **kiwi** | 12 |";
        assert_eq!(
            text(md, 80),
            ["┌───────┬─────┐", "│ Name  │ Qty │", "├───────┼─────┤", "│ apple │   3 │", "│ kiwi  │  12 │", "└───────┴─────┘"]
        );
        let narrow = text("| a | long long long long text here |\n|---|---|\n| b | more more more more words |", 24);
        assert!(narrow.iter().all(|l| l.width() <= 24), "{narrow:?}");
        assert!(narrow.iter().any(|l| l.contains('…')));
        assert_eq!(text("a | b", 80), ["a | b"], "no separator row, so not a table");
    }

    #[test]
    fn no_line_ever_exceeds_the_width() {
        let doc = "# Heading with several words in it\n\nA paragraph with **bold words that wrap** and `inline code spans`, a [link](https://example.com/very/long/path) and supercalifragilisticexpialidocious_tokens_without_spaces.\n\n- bullet one is long enough to wrap around the edge\n  - nested bullet also long enough to wrap around the edge\n1. numbered\n\n> quote that is long enough to wrap around the edge of the box\n\n```rust\nfn main() { println!(\"a very long line of code that needs breaking\"); }\n```\n\n| col a | col b | col c |\n|---|---|---|\n| some text | more text | even more text here |\n\n日本語のテキストは幅が二倍になります。これも折り返されるはずです。";
        for w in [6, 10, 17, 24, 40, 80] {
            for l in text(doc, w) {
                assert!(l.width() <= w, "width {w}: {l:?} is {}", l.width());
            }
        }
    }

    #[test]
    fn empty_and_blank_input_render_one_empty_line() {
        assert_eq!(text("", 40), [""]);
        assert_eq!(text("\n\n", 40), [""]);
        assert_eq!(text("a\n\n\n\nb", 40), ["a", "", "b"]);
    }
}
