//! Helix-style editing for the message box (input mode "helix"): selection first. A motion selects, then `d`,
//! `c`, `y` act on the selection. Lives inside `Vim` so the app drives both the same way (modes, label, cursor).
//! Not here: multiple selections, `s`/`S`, surround (`ms`), registers, `.`, macros, `:` commands.

use super::*;

/// The keys of one command: `[count] key`, or a prefix key (`g`, `m`, `f`, `t`, `r`...) and what it needs.
enum H<'a> {
    More,
    Run(Option<usize>, &'a [char]),
}

fn parse(k: &[char]) -> H<'_> {
    let mut i = 0;
    let n = count(k, &mut i);
    let rest = &k[i..];
    let need = match rest.first() {
        None => return H::More,
        Some('g' | 'f' | 't' | 'F' | 'T' | 'r') => 2,
        Some('m') => 3,
        _ => 1,
    };
    if rest.len() < need { H::More } else { H::Run(n, rest) }
}

impl Vim {
    /// The selection as the inclusive span `[lo, hi]`, hi clamped onto the text.
    fn span(&self, t: &[char], cur: usize) -> (usize, usize) {
        let (a, b) = (self.anchor.min(cur), self.anchor.max(cur));
        (a, b.min(t.len().saturating_sub(1)))
    }

    /// Select from `from` to `to`; in select mode (`v`) only the head moves.
    fn pick(&mut self, from: usize, to: usize, cur: &mut usize) {
        if self.mode != Mode::Visual {
            self.anchor = from;
        }
        *cur = to;
    }

    pub(super) fn helix_key(&mut self, k: KeyEvent, t: &mut Vec<char>, cur: &mut usize) -> Out {
        let (ctrl, alt) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT));
        if *cur != self.head {
            self.anchor = *cur; // the app moved the cursor (history, a click): that is a fresh selection
        }
        let c = match k.code {
            KeyCode::Esc if self.mode == Mode::Visual || self.pending() => {
                self.keys.clear();
                self.mode = Mode::Normal;
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
        let H::Run(n, rest) = parse(&self.keys) else { return Out::Done };
        let rest = rest.to_vec();
        self.keys.clear();
        let out = self.helix_run(n.unwrap_or(1), &rest, t, cur);
        self.settle(t, cur);
        self.head = *cur;
        out
    }

    fn helix_run(&mut self, n: usize, k: &[char], t: &mut Vec<char>, cur: &mut usize) -> Out {
        let len = t.len();
        let (lo, hi) = self.span(t, *cur);
        let (ls, le) = (line_start(t, *cur), line_end(t, *cur));
        match k {
            ['h'] => self.pick(cur.saturating_sub(n).max(ls), cur.saturating_sub(n).max(ls), cur),
            ['l'] => {
                let to = (*cur + n).min(le.saturating_sub(1).max(ls));
                self.pick(to, to, cur)
            }
            ['j'] => return Out::Row(n as isize),
            ['k'] => return Out::Row(-(n as isize)),
            ['w' | 'W'] | ['e' | 'E'] | ['b' | 'B'] => {
                let big = k[0].is_uppercase();
                let (mut from, mut head) = (*cur, *cur);
                for i in 0..n {
                    let s = if t.get(head).is_some_and(|c| c.is_whitespace()) && k[0].eq_ignore_ascii_case(&'w') { head + 1 } else { head };
                    if i == 0 {
                        from = s;
                    }
                    head = match k[0].to_ascii_lowercase() {
                        'w' => word_fwd(t, s.min(len), big).saturating_sub(1).max(s),
                        'e' => word_end(t, s, big),
                        _ => word_back(t, s, big),
                    };
                }
                self.pick(if k[0].eq_ignore_ascii_case(&'b') { *cur } else { from }, head, cur);
            }
            ['g', 'h'] => self.pick(ls, ls, cur),
            ['g', 'l'] => {
                let to = le.saturating_sub(1).max(ls);
                self.pick(to, to, cur)
            }
            ['g', 's'] => {
                let to = first_word(t, *cur);
                self.pick(to, to, cur)
            }
            ['g', 'g'] => self.pick(0, 0, cur),
            ['g', 'e'] | ['G'] => {
                let to = len.saturating_sub(1);
                self.pick(to, to, cur)
            }
            [c @ ('f' | 't' | 'F' | 'T'), ch] => {
                let (fwd, till) = (c.is_lowercase(), matches!(c, 't' | 'T'));
                self.find = Some((*ch, fwd, till));
                if let Some(tg) = self.find_from(t, *cur, *ch, fwd, till, n, false) {
                    self.pick(*cur, tg.pos, cur);
                }
            }
            ['x'] => {
                // A selection of whole lines (an earlier x) grows by a line; anything else becomes its line.
                let lines = lo == line_start(t, lo) && (t.get(hi) == Some(&'\n') || hi + 1 >= len);
                let a = if self.mode == Mode::Visual { self.anchor } else if lines { lo } else { line_start(t, lo) };
                let mut b = hi;
                for i in 0..n {
                    if i > 0 || lines {
                        if b + 1 >= len {
                            break;
                        }
                        b += 1;
                    }
                    b = line_end(t, b).min(len.saturating_sub(1));
                }
                self.anchor = a;
                *cur = b;
            }
            ['%'] => {
                self.anchor = 0;
                *cur = len.saturating_sub(1);
            }
            [';'] => self.anchor = *cur,
            ['v'] => self.mode = if self.mode == Mode::Visual { Mode::Normal } else { Mode::Visual },
            ['m', w @ ('i' | 'a'), o] => {
                if let Some((a, b)) = object(t, *cur, *w == 'i', *o) {
                    self.anchor = a;
                    *cur = b.saturating_sub(1).max(a);
                }
            }
            ['d'] | ['c'] | ['y'] if len > 0 => {
                self.reg = (t[lo..=hi].iter().collect(), false);
                if k[0] != 'y' {
                    t.drain(lo..=hi);
                    *cur = lo;
                    self.anchor = lo;
                    self.mode = if k[0] == 'c' { Mode::Insert } else { Mode::Normal };
                }
            }
            ['p' | 'P'] => {
                let chars: Vec<char> = self.reg.0.chars().collect();
                if !chars.is_empty() {
                    let at = if k[0] == 'p' { (hi + 1).min(len) } else { lo };
                    let all: Vec<char> = chars.iter().copied().cycle().take(chars.len() * n).collect();
                    t.splice(at..at, all.iter().copied());
                    self.anchor = at;
                    *cur = at + all.len() - 1;
                }
            }
            ['r', ch] if len > 0 => {
                t[lo..=hi].iter_mut().filter(|c| **c != '\n').for_each(|c| *c = *ch);
                self.anchor = lo;
                *cur = hi;
            }
            ['~'] if len > 0 => {
                t[lo..=hi].iter_mut().for_each(|c| *c = toggle(*c));
            }
            ['i'] => {
                *cur = lo;
                self.mode = Mode::Insert;
            }
            ['a'] => {
                *cur = (hi + 1).min(len);
                self.mode = Mode::Insert;
            }
            ['I'] => {
                *cur = first_word(t, *cur);
                self.mode = Mode::Insert;
            }
            ['A'] => {
                *cur = le;
                self.mode = Mode::Insert;
            }
            ['o'] => {
                let e = line_end(t, hi);
                t.insert(e, '\n');
                *cur = e + 1;
                self.mode = Mode::Insert;
            }
            ['O'] => {
                let s = line_start(t, lo);
                t.insert(s, '\n');
                *cur = s;
                self.mode = Mode::Insert;
            }
            ['u'] => return Out::Undo,
            ['U'] => return Out::Redo,
            ['/'] => return Out::Search,
            _ => {}
        }
        if self.mode == Mode::Insert {
            self.anchor = *cur;
        }
        Out::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type `keys` (`\x1b` is Esc) in normal mode on `text`, the cursor at `at`; returns text, selection and mode.
    fn run(keys: &str, text: &str, at: usize) -> (String, (usize, usize), Mode) {
        let mut v = Vim { on: true, helix: true, mode: Mode::Normal, anchor: at, head: at, ..Default::default() };
        let (mut t, mut cur): (Vec<char>, usize) = (text.chars().collect(), at);
        for c in keys.chars() {
            let code = if c == '\x1b' { KeyCode::Esc } else { KeyCode::Char(c) };
            if v.mode == Mode::Insert && c != '\x1b' {
                t.insert(cur, c);
                cur += 1;
                continue;
            }
            v.key(KeyEvent::new(code, KeyModifiers::NONE), &mut t, &mut cur);
        }
        (t.iter().collect(), (v.anchor.min(cur), v.anchor.max(cur)), v.mode)
    }

    fn text(keys: &str, text: &str, at: usize) -> String {
        run(keys, text, at).0
    }

    #[test]
    fn motions_select_and_the_verb_acts_on_the_selection() {
        assert_eq!(run("w", "one two three", 0).1, (0, 3), "w selects the word and the space after it");
        assert_eq!(run("ww", "one two three", 0).1, (4, 7));
        assert_eq!(run("e", "one two", 0).1, (0, 2));
        assert_eq!(run("b", "one two", 6).1, (4, 6));
        assert_eq!(run("wd", "one two", 0).0, "two");
        assert_eq!(text("wd", "one two three", 0), "two three");
        assert_eq!(text("ecX\x1b", "one two", 0), "X two");
        assert_eq!(text("d", "abc", 1), "ac", "a lone cursor is a one-character selection");
        assert_eq!(text("xd", "a\nb\nc", 2), "a\nc");
        assert_eq!(text("xxd", "a\nb\nc", 0), "c");
        assert_eq!(text("xd", "a\nb", 2), "a\n", "the last line leaves the break before it");
        assert_eq!(text("%d", "all of it", 3), "");
        assert_eq!(text("3ld", "abcdef", 0), "abcef", "3l moves; d takes the one character under the cursor");
    }

    #[test]
    fn select_mode_extends_and_find_motions_select_through_the_character() {
        assert_eq!(run("vll", "abcd", 0).1, (0, 2));
        assert_eq!(text("vlld", "abcd", 0), "d");
        assert_eq!(run("vl\x1b", "abcd", 0).2, Mode::Normal);
        assert_eq!(run("f,", "a,b", 0).1, (0, 1));
        assert_eq!(text("t,d", "ab,cd", 0), ",cd");
        assert_eq!(run("gl", "abc\nd", 0).1, (2, 2));
        assert_eq!(run("gh", "abc", 2).1, (0, 0));
        assert_eq!(run("ge", "abc\nd", 0).1, (4, 4));
        assert_eq!(run("l;", "abcd", 0).1, (1, 1), "; collapses a selection");
    }

    #[test]
    fn yank_paste_insert_and_objects() {
        assert_eq!(text("wyp", "one two", 0), "one one two");
        assert_eq!(text("yP", "ab", 1), "abb");
        assert_eq!(text("iX\x1b", "ab", 1), "aXb");
        assert_eq!(text("aX\x1b", "ab", 0), "aXb");
        assert_eq!(text("AX\x1b", "ab\ncd", 0), "abX\ncd");
        assert_eq!(text("ox\x1b", "a\nb", 0), "a\nx\nb");
        assert_eq!(text("Ox\x1b", "a\nb", 2), "a\nx\nb");
        assert_eq!(text("miwd", "one two", 5), "one ");
        assert_eq!(text("mi(d", "f(a, b)", 3), "f()");
        assert_eq!(text("rx", "abc", 1), "axc");
        assert_eq!(text("w~", "ab cd", 0), "AB cd");
        assert_eq!(text("cX\x1b", "abc", 1), "aXc");
    }

    #[test]
    fn the_app_gets_what_is_not_editing() {
        let mut v = Vim { on: true, helix: true, mode: Mode::Normal, ..Default::default() };
        let (mut t, mut cur): (Vec<char>, usize) = ("ab".chars().collect(), 0);
        let mut key = |c: KeyCode, m| v.key(KeyEvent::new(c, m), &mut t, &mut cur);
        assert_eq!(key(KeyCode::Enter, KeyModifiers::NONE), Out::Pass);
        assert_eq!(key(KeyCode::Esc, KeyModifiers::NONE), Out::Pass, "esc in normal mode interrupts or rewinds");
        assert_eq!(key(KeyCode::Char('j'), KeyModifiers::NONE), Out::Row(1));
        assert_eq!(key(KeyCode::Char('u'), KeyModifiers::NONE), Out::Undo);
        assert_eq!(key(KeyCode::Char('U'), KeyModifiers::NONE), Out::Redo);
        assert_eq!(key(KeyCode::Char('/'), KeyModifiers::NONE), Out::Search);
    }
}
