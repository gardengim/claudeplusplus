//! Vim-style editing for the message box (input mode "vim"; "helix" is helix.rs). Normal and visual mode work on the text and cursor they are
//! handed, so they need no terminal; the app keeps what is not editing (history, undo, sending).
//! Not here: `.` repeat, macros, named registers, marks, `:` commands.

mod helix;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Mode {
    #[default]
    Insert,
    Normal,
    Visual,
    VisualLine,
}

/// What the app does after a key: `Done` means the key was used up here, `Pass` that it is the app's.
#[derive(PartialEq, Eq, Debug)]
pub enum Out {
    Done,
    Pass,
    /// Move the cursor this many rows (negative: up) in the box as drawn, wrapped rows included.
    Row(isize),
    Undo,
    Redo,
    Search,
}

#[derive(Default)]
pub struct Vim {
    /// Modal editing is on (input mode vim or helix).
    pub on: bool,
    /// ...and it is helix's selection-first kind (see helix.rs).
    pub helix: bool,
    /// Where the cursor was left, to notice the app moving it (helix).
    head: usize,
    pub mode: Mode,
    keys: Vec<char>,
    anchor: usize,
    /// The unnamed register: the text, and whether it is whole lines.
    reg: (String, bool),
    /// The last f/F/t/T: the character, forward, till.
    find: Option<(char, bool, bool)>,
}

#[derive(Clone, Copy)]
enum Mv {
    Left,
    Right,
    Down,
    Up,
    Home,
    First,
    End,
    Word(bool),
    Back(bool),
    WordEnd(bool),
    Top,
    Bottom,
    Find(char, bool, bool),
    Again(bool),
}

enum Act {
    Move(Mv),
    /// `dd`, `yy`, `cc`
    Line,
    /// `iw`, `a"`: inner or around, and which object
    Obj(bool, char),
    Plain(char),
    Replace(char),
}

struct Cmd {
    n: Option<usize>,
    op: Option<char>,
    act: Act,
}

enum P {
    More,
    Bad,
    Run(Cmd),
}

/// Where a motion lands, and how an operator should take it.
struct Tgt {
    pos: usize,
    /// The character at `pos` is part of the span.
    incl: bool,
    /// Whole lines.
    line: bool,
}

fn motion(c: char) -> Option<Mv> {
    Some(match c {
        'h' => Mv::Left,
        'l' => Mv::Right,
        'j' => Mv::Down,
        'k' => Mv::Up,
        '0' => Mv::Home,
        '^' => Mv::First,
        '$' => Mv::End,
        'w' => Mv::Word(false),
        'W' => Mv::Word(true),
        'b' => Mv::Back(false),
        'B' => Mv::Back(true),
        'e' => Mv::WordEnd(false),
        'E' => Mv::WordEnd(true),
        'G' => Mv::Bottom,
        ';' => Mv::Again(false),
        ',' => Mv::Again(true),
        _ => return None,
    })
}

fn count(k: &[char], i: &mut usize) -> Option<usize> {
    let s = *i;
    while k.get(*i).is_some_and(|c| c.is_ascii_digit() && (*i > s || *c != '0')) {
        *i += 1;
    }
    (*i > s).then(|| k[s..*i].iter().collect::<String>().parse().unwrap_or(1))
}

/// `[count] [d|c|y] [count] motion | object | line`, or a plain command. `visual`: d, c and y act on the selection.
fn parse(k: &[char], visual: bool) -> P {
    let mut i = 0;
    let n1 = count(k, &mut i);
    let Some(&first) = k.get(i) else { return P::More };
    i += 1;
    let (mut op, mut n2, mut c) = (None, None, first);
    if !visual && matches!(c, 'd' | 'c' | 'y') {
        op = Some(c);
        n2 = count(k, &mut i);
        let Some(&c2) = k.get(i) else { return P::More };
        i += 1;
        c = c2;
    }
    let n = (n1.is_some() || n2.is_some()).then(|| n1.unwrap_or(1) * n2.unwrap_or(1));
    let arg = k.get(i).copied();
    let act = match c {
        c if Some(c) == op => Act::Line,
        'i' | 'a' if op.is_some() || visual => match arg {
            Some(o) => Act::Obj(c == 'i', o),
            None => return P::More,
        },
        'g' => match arg {
            None => return P::More,
            Some('g') => Act::Move(Mv::Top),
            _ => return P::Bad,
        },
        'f' | 'F' | 't' | 'T' => match arg {
            Some(x) => Act::Move(Mv::Find(x, c.is_lowercase(), matches!(c, 't' | 'T'))),
            None => return P::More,
        },
        'r' if op.is_none() => match arg {
            Some(x) => Act::Replace(x),
            None => return P::More,
        },
        c => match motion(c) {
            Some(m) => Act::Move(m),
            None if op.is_none() => Act::Plain(c),
            None => return P::Bad,
        },
    };
    P::Run(Cmd { n, op, act })
}

fn line_start(t: &[char], i: usize) -> usize {
    t[..i.min(t.len())].iter().rposition(|c| *c == '\n').map_or(0, |p| p + 1)
}

fn line_end(t: &[char], i: usize) -> usize {
    let i = i.min(t.len());
    t[i..].iter().position(|c| *c == '\n').map_or(t.len(), |p| i + p)
}

fn first_word(t: &[char], i: usize) -> usize {
    let (a, b) = (line_start(t, i), line_end(t, i));
    (a..b).find(|&j| !t[j].is_whitespace()).unwrap_or(a)
}

fn cls(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

fn word_fwd(t: &[char], mut i: usize, big: bool) -> usize {
    let len = t.len();
    if i >= len {
        return len;
    }
    let c = cls(t[i], big);
    while c != 0 && i < len && cls(t[i], big) == c {
        i += 1;
    }
    while i < len && t[i].is_whitespace() {
        i += 1;
    }
    i
}

fn word_back(t: &[char], mut i: usize, big: bool) -> usize {
    while i > 0 && t[i - 1].is_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let c = cls(t[i - 1], big);
    while i > 0 && cls(t[i - 1], big) == c {
        i -= 1;
    }
    i
}

fn word_end(t: &[char], i: usize, big: bool) -> usize {
    let len = t.len();
    let mut i = i + 1;
    while i < len && t[i].is_whitespace() {
        i += 1;
    }
    if i >= len {
        return len.saturating_sub(1);
    }
    let c = cls(t[i], big);
    while i + 1 < len && cls(t[i + 1], big) == c {
        i += 1;
    }
    i
}

/// The span `[a, b)` an `i`/`a` object names around `cur`.
fn object(t: &[char], cur: usize, inner: bool, o: char) -> Option<(usize, usize)> {
    let len = t.len();
    if cur >= len {
        return None;
    }
    match o {
        'w' | 'W' => {
            let big = o == 'W';
            let c = cls(t[cur], big);
            let (mut a, mut b) = (cur, cur + 1);
            while a > 0 && cls(t[a - 1], big) == c {
                a -= 1;
            }
            while b < len && cls(t[b], big) == c {
                b += 1;
            }
            if !inner {
                let (la, lb) = (line_start(t, cur), line_end(t, cur));
                let mut e = b;
                while e < lb && t[e].is_whitespace() && t[e] != '\n' {
                    e += 1;
                }
                if e > b {
                    b = e;
                } else {
                    while a > la && t[a - 1].is_whitespace() {
                        a -= 1;
                    }
                }
            }
            Some((a, b))
        }
        '"' | '\'' | '`' => {
            let (la, lb) = (line_start(t, cur), line_end(t, cur));
            let marks: Vec<usize> = (la..lb).filter(|&j| t[j] == o).collect();
            // Quotes pair up from the left; the pair around the cursor, else the next one.
            let (a, b) = marks.windows(2).step_by(2).map(|p| (p[0], p[1])).find(|p| cur <= p.1)?;
            Some(if inner { (a + 1, b) } else { (a, b + 1) })
        }
        '(' | ')' | 'b' | '[' | ']' | '{' | '}' | 'B' | '<' | '>' => {
            let (open, close) = match o {
                '(' | ')' | 'b' => ('(', ')'),
                '[' | ']' => ('[', ']'),
                '{' | '}' | 'B' => ('{', '}'),
                _ => ('<', '>'),
            };
            let mut depth = 0;
            let mut a = None;
            for j in (0..=cur).rev() {
                if t[j] == close && j != cur {
                    depth += 1;
                } else if t[j] == open {
                    if depth == 0 {
                        a = Some(j);
                        break;
                    }
                    depth -= 1;
                }
            }
            let a = a?;
            let mut depth = 0;
            let b = (a + 1..len).find(|&j| {
                if t[j] == open {
                    depth += 1;
                } else if t[j] == close {
                    if depth == 0 {
                        return true;
                    }
                    depth -= 1;
                }
                false
            })?;
            Some(if inner { (a + 1, b) } else { (a, b + 1) })
        }
        _ => None,
    }
}

impl Vim {
    pub fn visual(&self) -> bool {
        matches!(self.mode, Mode::Visual | Mode::VisualLine)
    }

    /// A normal-mode command is half typed (`d`, `2`, `f`): the next key belongs to it.
    pub fn pending(&self) -> bool {
        !self.keys.is_empty()
    }

    pub fn label(&self) -> &'static str {
        match self.mode {
            Mode::Insert => "INSERT",
            Mode::Normal => "NORMAL",
            Mode::Visual if self.helix => "SELECT",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        }
    }

    /// The cursor sits on a character in normal and visual mode, never past the last one or on a line break.
    pub fn settle(&self, t: &[char], cur: &mut usize) {
        if self.mode == Mode::Insert {
            return;
        }
        if self.helix {
            *cur = (*cur).min(t.len().saturating_sub(1)); // helix may stand on a line break
            return;
        }
        *cur = (*cur).min(t.len());
        if *cur == t.len() && *cur > 0 {
            *cur -= 1;
        }
        if t.get(*cur) == Some(&'\n') && *cur > 0 && t[*cur - 1] != '\n' {
            *cur -= 1;
        }
    }

    /// The app moved the cursor itself (rows as drawn): helix selects from where it was left.
    pub fn moved(&mut self, cur: usize) {
        if self.helix && self.mode == Mode::Normal {
            self.anchor = cur;
        }
        self.head = cur;
    }

    /// The selection as a span `[a, b)`.
    fn selection(&self, t: &[char], cur: usize) -> (usize, usize, bool) {
        let (a, b) = (self.anchor.min(cur), self.anchor.max(cur));
        if self.mode == Mode::VisualLine {
            (line_start(t, a), line_end(t, b), true)
        } else {
            (a, (b + 1).min(t.len()), false)
        }
    }

    /// The selection, for drawing: `[a, b)` in characters, and whether it is whole lines.
    pub fn highlight(&self, t: &[char], cur: usize) -> Option<(usize, usize)> {
        if self.helix {
            let (a, b) = (self.anchor.min(cur), self.anchor.max(cur));
            return (self.mode != Mode::Insert && a != b && cur == self.head).then_some((a, (b + 1).min(t.len())));
        }
        self.visual().then(|| {
            let (a, b, _) = self.selection(t, cur);
            (a, b)
        })
    }

    pub fn key(&mut self, k: KeyEvent, t: &mut Vec<char>, cur: &mut usize) -> Out {
        let (ctrl, alt) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT));
        if self.mode == Mode::Insert {
            if k.code != KeyCode::Esc {
                return Out::Pass;
            }
            if *cur > 0 && t.get(*cur - 1).is_some_and(|c| *c != '\n') {
                *cur -= 1;
            }
            self.mode = Mode::Normal;
            self.settle(t, cur);
            (self.anchor, self.head) = (*cur, *cur);
            return Out::Done;
        }
        if self.helix {
            return self.helix_key(k, t, cur);
        }
        let c = match k.code {
            KeyCode::Esc if self.mode != Mode::Normal || self.pending() => {
                self.keys.clear();
                self.mode = Mode::Normal;
                self.settle(t, cur);
                return Out::Done;
            }
            KeyCode::Char('r') if ctrl => return Out::Redo,
            KeyCode::Char(c) if !ctrl && !alt => c,
            KeyCode::Left if !self.pending() => 'h',
            KeyCode::Backspace if !self.pending() => 'h',
            KeyCode::Right if !self.pending() => 'l',
            _ => return Out::Pass,
        };
        self.keys.push(c);
        match parse(&self.keys, self.visual()) {
            P::More => Out::Done,
            P::Bad => {
                self.keys.clear();
                Out::Done
            }
            P::Run(cmd) => {
                self.keys.clear();
                let out = self.run(cmd, t, cur);
                self.settle(t, cur);
                out
            }
        }
    }

    /// Where `m` takes the cursor `n` times over, if it can.
    fn target(&self, m: Mv, t: &[char], cur: usize, n: usize, op: bool) -> Option<Tgt> {
        let len = t.len();
        let to = |pos, incl, line| Some(Tgt { pos, incl, line });
        match m {
            Mv::Left => to(cur.saturating_sub(n).max(line_start(t, cur)), false, false),
            Mv::Right => to((cur + n).min(line_end(t, cur)), false, false),
            Mv::Home => to(line_start(t, cur), false, false),
            Mv::First => to(first_word(t, cur), false, false),
            Mv::End => {
                let mut e = line_end(t, cur);
                for _ in 1..n {
                    e = line_end(t, (e + 1).min(len));
                }
                to(e, false, false)
            }
            Mv::Word(big) => to((0..n).fold(cur, |i, _| word_fwd(t, i, big)), false, false),
            Mv::Back(big) => to((0..n).fold(cur, |i, _| word_back(t, i, big)), false, false),
            Mv::WordEnd(big) => to((0..n).fold(cur, |i, _| word_end(t, i, big)), true, false),
            Mv::Top => to(0, false, true),
            Mv::Bottom => to(line_start(t, len), false, true),
            Mv::Down | Mv::Up => {
                if !op {
                    return None; // the app moves by drawn rows
                }
                let mut at = cur;
                for _ in 0..n {
                    at = if matches!(m, Mv::Down) {
                        let e = line_end(t, at);
                        if e >= len { at } else { e + 1 }
                    } else {
                        let s = line_start(t, at);
                        if s == 0 { at } else { line_start(t, s - 1) }
                    };
                }
                to(at, false, true)
            }
            Mv::Find(ch, fwd, till) => self.find_from(t, cur, ch, fwd, till, n, false),
            Mv::Again(rev) => {
                let (ch, fwd, till) = self.find?;
                self.find_from(t, cur, ch, fwd != rev, till, n, true)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn find_from(&self, t: &[char], cur: usize, ch: char, fwd: bool, till: bool, n: usize, again: bool) -> Option<Tgt> {
        let (a, b) = (line_start(t, cur), line_end(t, cur));
        // Repeating a till must not stay stuck beside its character.
        let from = match (again && till, fwd) {
            (true, true) => cur + 1,
            (true, false) => cur.saturating_sub(1),
            _ => cur,
        };
        let pos = if fwd {
            (from + 1..b).filter(|&j| t[j] == ch).nth(n - 1)?
        } else {
            (a..from).rev().filter(|&j| t[j] == ch).nth(n - 1)?
        };
        let pos = match (till, fwd) {
            (true, true) => pos - 1,
            (true, false) => pos + 1,
            _ => pos,
        };
        Some(Tgt { pos, incl: fwd, line: false })
    }

    fn run(&mut self, c: Cmd, t: &mut Vec<char>, cur: &mut usize) -> Out {
        let n = c.n.unwrap_or(1);
        let visual = self.visual();
        if let Act::Move(Mv::Find(ch, fwd, till)) = c.act {
            self.find = Some((ch, fwd, till));
        }
        match (c.op, c.act) {
            (None, Act::Move(Mv::Down)) => return Out::Row(n as isize),
            (None, Act::Move(Mv::Up)) => return Out::Row(-(n as isize)),
            (None, Act::Move(m)) => {
                if let Some(tg) = self.target(m, t, *cur, n, false) {
                    *cur = tg.pos;
                }
            }
            (None, Act::Obj(inner, o)) => {
                if let Some((a, b)) = object(t, *cur, inner, o).filter(|_| visual) {
                    (self.anchor, *cur) = (a, b.saturating_sub(1).max(a));
                }
            }
            (Some(op), Act::Move(m)) => {
                // `cw` changes to the end of the word, as `ce` does.
                let m = match m {
                    Mv::Word(big) if op == 'c' && t.get(*cur).is_some_and(|x| !x.is_whitespace()) => Mv::WordEnd(big),
                    m => m,
                };
                let Some(tg) = self.target(m, t, *cur, n, true) else { return Out::Done };
                let (mut a, mut b) = (tg.pos.min(*cur), tg.pos.max(*cur));
                if tg.incl && tg.pos >= *cur {
                    b += 1;
                }
                if matches!(m, Mv::Word(_)) && n == 1 {
                    b = b.min(line_end(t, a)); // `dw` on a line's last word keeps the line break
                }
                if tg.line {
                    (a, b) = (line_start(t, a), line_end(t, b));
                }
                self.operate(op, a, b.min(t.len()), tg.line, t, cur);
            }
            (Some(op), Act::Line) => {
                let mut e = *cur;
                for _ in 1..n {
                    let end = line_end(t, e);
                    if end >= t.len() {
                        break;
                    }
                    e = end + 1;
                }
                self.operate(op, line_start(t, *cur), line_end(t, e), true, t, cur);
            }
            (Some(op), Act::Obj(inner, o)) => {
                if let Some((a, b)) = object(t, *cur, inner, o) {
                    self.operate(op, a, b, false, t, cur);
                }
            }
            (_, Act::Replace(ch)) => {
                let end = (*cur + n).min(line_end(t, *cur));
                if end - *cur == n {
                    t[*cur..end].fill(ch);
                    *cur = end - 1;
                }
            }
            (_, Act::Line) => {}
            (None, Act::Plain(p)) => return self.plain(p, n, t, cur),
            (Some(_), Act::Plain(_)) => {}
        }
        Out::Done
    }

    fn operate(&mut self, op: char, a: usize, b: usize, line: bool, t: &mut Vec<char>, cur: &mut usize) {
        self.reg = (t[a..b].iter().collect(), line);
        match op {
            'y' => {
                if !line || a > *cur {
                    *cur = a;
                }
            }
            'c' => {
                t.drain(a..b);
                *cur = a;
                self.mode = Mode::Insert;
            }
            _ => {
                if line && b < t.len() {
                    t.drain(a..=b);
                    *cur = a;
                } else if line && a > 0 {
                    t.drain(a - 1..b);
                    *cur = line_start(t, a - 1);
                } else {
                    t.drain(a..b);
                    *cur = a;
                }
            }
        }
        if self.visual() && self.mode != Mode::Insert {
            self.mode = Mode::Normal;
        }
    }

    fn plain(&mut self, p: char, n: usize, t: &mut Vec<char>, cur: &mut usize) -> Out {
        let len = t.len();
        if self.visual() {
            let (a, b, line) = self.selection(t, *cur);
            match p {
                'd' | 'x' | 'X' | 'D' => self.operate('d', a, b, line, t, cur),
                'y' => {
                    self.operate('y', a, b, line, t, cur);
                    *cur = a;
                }
                'c' | 's' | 'C' | 'S' => self.operate('c', a, b, line, t, cur),
                '~' => {
                    for ch in &mut t[a..b] {
                        *ch = toggle(*ch);
                    }
                    *cur = a;
                    self.mode = Mode::Normal;
                }
                'o' => std::mem::swap(&mut self.anchor, cur),
                'v' => self.mode = if self.mode == Mode::Visual { Mode::Normal } else { Mode::Visual },
                'V' => self.mode = if self.mode == Mode::VisualLine { Mode::Normal } else { Mode::VisualLine },
                _ => {}
            }
            return Out::Done;
        }
        let end = line_end(t, *cur);
        match p {
            'x' => self.operate('d', *cur, (*cur + n).min(end), false, t, cur),
            'X' => self.operate('d', cur.saturating_sub(n).max(line_start(t, *cur)), *cur, false, t, cur),
            'D' => self.operate('d', *cur, end, false, t, cur),
            'C' => self.operate('c', *cur, end, false, t, cur),
            's' => self.operate('c', *cur, (*cur + n).min(end), false, t, cur),
            'S' => self.operate('c', line_start(t, *cur), end, true, t, cur),
            'Y' => self.operate('y', line_start(t, *cur), end, true, t, cur),
            'i' => self.mode = Mode::Insert,
            'a' => {
                *cur = (*cur + 1).min(end);
                self.mode = Mode::Insert;
            }
            'I' => {
                *cur = first_word(t, *cur);
                self.mode = Mode::Insert;
            }
            'A' => {
                *cur = end;
                self.mode = Mode::Insert;
            }
            'o' => {
                t.insert(end, '\n');
                *cur = end + 1;
                self.mode = Mode::Insert;
            }
            'O' => {
                let s = line_start(t, *cur);
                t.insert(s, '\n');
                *cur = s;
                self.mode = Mode::Insert;
            }
            'p' | 'P' => {
                let (text, line) = self.reg.clone();
                if text.is_empty() && !line {
                    return Out::Done;
                }
                let chars: Vec<char> = text.chars().collect();
                for _ in 0..n {
                    if line {
                        let at = if p == 'p' { end } else { line_start(t, *cur) };
                        let mut ins = chars.clone();
                        if p == 'p' {
                            ins.insert(0, '\n');
                        } else {
                            ins.push('\n');
                        }
                        let start = at + usize::from(p == 'p');
                        t.splice(at..at, ins);
                        *cur = start;
                    } else {
                        let at = if p == 'p' && len > 0 && t.get(*cur).is_some_and(|c| *c != '\n') { *cur + 1 } else { *cur };
                        t.splice(at..at, chars.iter().copied());
                        *cur = at + chars.len().saturating_sub(1);
                    }
                }
            }
            'J' => {
                // Join with the next line: the break and the next line's indent become one space.
                if end < len {
                    let mut e = end + 1;
                    while e < len && t[e].is_whitespace() && t[e] != '\n' {
                        e += 1;
                    }
                    t.splice(end..e, [' ']);
                    *cur = end;
                }
            }
            '~' => {
                let e = (*cur + n).min(end);
                for ch in &mut t[*cur..e] {
                    *ch = toggle(*ch);
                }
                *cur = e;
            }
            'v' | 'V' => {
                self.anchor = *cur;
                self.mode = if p == 'v' { Mode::Visual } else { Mode::VisualLine };
            }
            'u' => return Out::Undo,
            '/' => return Out::Search,
            _ => {}
        }
        Out::Done
    }
}

fn toggle(c: char) -> char {
    if c.is_lowercase() { c.to_uppercase().next().unwrap_or(c) } else { c.to_lowercase().next().unwrap_or(c) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type `keys` (`\x1b` is Esc) into normal mode on `text`, the cursor at `at`; returns text and cursor.
    fn run(keys: &str, text: &str, at: usize) -> (String, usize, Mode) {
        let mut v = Vim { on: true, mode: Mode::Normal, ..Default::default() };
        let (mut t, mut cur): (Vec<char>, usize) = (text.chars().collect(), at);
        for c in keys.chars() {
            let code = if c == '\x1b' { KeyCode::Esc } else { KeyCode::Char(c) };
            if v.mode == Mode::Insert && c != '\x1b' {
                t.insert(cur, c); // what the app does for text in insert mode
                cur += 1;
                continue;
            }
            v.key(KeyEvent::new(code, KeyModifiers::NONE), &mut t, &mut cur);
        }
        (t.iter().collect(), cur, v.mode)
    }

    fn text(keys: &str, text: &str, at: usize) -> String {
        run(keys, text, at).0
    }

    #[test]
    fn deletes_changes_and_yanks_by_motion_count_and_line() {
        assert_eq!(text("dw", "one two three", 4), "one three");
        assert_eq!(text("dw", "one two\nnext", 4), "one \nnext", "the line break stays");
        assert_eq!(text("2dw", "a b c d", 0), "c d");
        assert_eq!(text("d$", "keep drop", 5), "keep ");
        assert_eq!(text("d0", "drop keep", 5), "keep");
        assert_eq!(text("cwnew\x1b", "old word", 0), "new word");
        assert_eq!(text("dd", "a\nb\nc", 2), "a\nc");
        assert_eq!(text("dd", "a\nb\nc", 4), "a\nb", "the last line takes the break before it");
        assert_eq!(text("2dd", "a\nb\nc", 0), "c");
        assert_eq!(text("ccx\x1b", "a\nold\nc", 3), "a\nx\nc");
        assert_eq!(text("dj", "a\nb\nc", 0), "c");
        assert_eq!(text("yyp", "a\nb", 0), "a\na\nb");
        assert_eq!(text("ywP", "ab cd", 0), "ab ab cd");
        assert_eq!(text("x", "abc", 1), "ac");
        assert_eq!(text("3x", "abcdef", 1), "aef");
        assert_eq!(text("Xx", "abc", 2), "a");
        assert_eq!(text("D", "ab cd", 2), "ab");
        assert_eq!(text("Cz\x1b", "ab cd", 2), "abz");
        assert_eq!(text("xp", "ab", 0), "ba");
    }

    #[test]
    fn objects_and_finds_reach_inside_words_quotes_and_brackets() {
        assert_eq!(text("diw", "one two three", 5), "one  three");
        assert_eq!(text("daw", "one two three", 5), "one three");
        assert_eq!(text("ci\"x\x1b", "say \"hello there\" now", 7), "say \"x\" now");
        assert_eq!(text("da\"", "say \"hi\" now", 6), "say  now");
        assert_eq!(text("di(", "f(a, (b), c)", 8), "f()");
        assert_eq!(text("dib", "f(a, (b), c)", 6), "f(a, (), c)");
        assert_eq!(text("dt)", "f(abc)", 2), "f()");
        assert_eq!(text("df,", "a,b,c", 0), "b,c");
        assert_eq!(text("dfx", "a,b,c", 0), "a,b,c", "no such character: nothing happens");
        assert_eq!(text("f,;D", "a,b,c,d", 0), "a,b");
        assert_eq!(text("$F,D", "a,b,c", 4), "a,b");
    }

    #[test]
    fn insert_commands_and_the_small_edits() {
        assert_eq!(run("Ahi\x1b", "ab", 0), ("abhi".into(), 3, Mode::Normal));
        assert_eq!(text("Ix\x1b", "  ab", 3), "  xab");
        assert_eq!(text("ax\x1b", "ab", 0), "axb");
        assert_eq!(text("ox\x1b", "a\nb", 0), "a\nx\nb");
        assert_eq!(text("Ox\x1b", "a\nb", 2), "a\nx\nb");
        assert_eq!(text("3rx", "abcd", 0), "xxxd");
        assert_eq!(text("3rx", "ab", 0), "ab", "not enough characters: nothing happens");
        assert_eq!(text("~~", "ab", 0), "AB");
        assert_eq!(text("J", "a\n   b", 0), "a b");
        assert_eq!(text("sX\x1b", "abc", 1), "aXc");
        assert_eq!(text("SX\x1b", "ab\ncd", 0), "X\ncd");
    }

    #[test]
    fn motions_land_on_characters_and_escape_steps_back() {
        assert_eq!(run("w", "one two", 0).1, 4);
        assert_eq!(run("e", "one two", 0).1, 2);
        assert_eq!(run("b", "one two", 6).1, 4);
        assert_eq!(run("$", "abc", 0).1, 2, "never past the last character");
        assert_eq!(run("^", "  ab", 3).1, 2);
        assert_eq!(run("0", "  ab", 3).1, 0);
        assert_eq!(run("G", "a\nbc", 0).1, 2);
        assert_eq!(run("gg", "a\nbc", 3).1, 0);
        assert_eq!(run("5l", "abc", 0).1, 2);
        assert_eq!(run("ix\x1b", "ab", 1), ("axb".into(), 1, Mode::Normal), "Esc steps the cursor back");
        assert_eq!(run("d\x1b", "ab", 0).2, Mode::Normal);
    }

    #[test]
    fn visual_mode_selects_then_operates() {
        assert_eq!(text("vlld", "abcd", 0), "d");
        assert_eq!(text("vly", "abcd", 0), "abcd");
        assert_eq!(text("vlyP", "abcd", 2), "abcdcd", "yank, then paste before");
        assert_eq!(text("Vd", "a\nb\nc", 2), "a\nc");
        assert_eq!(text("viwd", "ab cd", 0), " cd");
        assert_eq!(text("vl~", "abc", 0), "ABc");
        assert_eq!(text("vlcX\x1b", "abc", 0), "Xc");
        assert_eq!(run("v\x1b", "abc", 1).2, Mode::Normal);
    }

    #[test]
    fn the_app_gets_what_is_not_editing() {
        let mut v = Vim { on: true, mode: Mode::Normal, ..Default::default() };
        let (mut t, mut cur): (Vec<char>, usize) = ("ab".chars().collect(), 0);
        let mut key = |c: KeyCode, m| v.key(KeyEvent::new(c, m), &mut t, &mut cur);
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE), Out::Pass);
        assert_eq!(key(KeyCode::Esc, KeyModifiers::NONE), Out::Pass, "esc in normal mode interrupts or rewinds");
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), Out::Pass);
        assert_eq!(key(KeyCode::Char('j'), KeyModifiers::NONE), Out::Row(1));
        assert_eq!(key(KeyCode::Char('3'), KeyModifiers::NONE), Out::Done);
        assert_eq!(key(KeyCode::Char('k'), KeyModifiers::NONE), Out::Row(-3));
        assert_eq!(key(KeyCode::Char('u'), KeyModifiers::NONE), Out::Undo);
        assert_eq!(key(KeyCode::Char('r'), KeyModifiers::CONTROL), Out::Redo);
        assert_eq!(key(KeyCode::Char('/'), KeyModifiers::NONE), Out::Search);
        assert_eq!(key(KeyCode::Char('z'), KeyModifiers::NONE), Out::Done, "an unknown key is swallowed, not typed");
    }
}
