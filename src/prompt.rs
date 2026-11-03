//! Prompts where the agent waits for you: permission requests and `AskUserQuestion`. Both are the same
//! component, a choice list, with the grammar every other picker uses: ↑↓ move (wrapping), Enter confirms,
//! Space toggles, 1-9 jump, Esc backs out (here: declines). Questions add tabs for several questions, a
//! review step, checkboxes for multi-select, an inline "Other" answer and option previews.
//!
//! State lives in [`Ui`]; [`Ui::question_key`] and [`Ui::permission_key`] are pure so they are easy to test,
//! and the `*_view` functions turn state into lines without touching the terminal.

use crate::panel::{View, row, row_with};
use crate::session::Choice;
use crate::theme;
use crate::ui::{truncate, wrap};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;
use serde_json::Value;

const PREVIEW_LINES: usize = 12;

/// Cut a line to `width` display columns, keeping each span's style.
fn clip(line: Line<'static>, width: usize) -> Line<'static> {
    let mut left = width;
    let mut spans = Vec::with_capacity(line.spans.len());
    for s in line.spans {
        if left == 0 {
            break;
        }
        let w = s.content.width();
        if w <= left {
            left -= w;
            spans.push(s);
        } else {
            let mut cut = String::new();
            for c in s.content.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if cw > left {
                    break;
                }
                cut.push(c);
                left -= cw;
            }
            spans.push(Span::styled(cut, s.style));
            break;
        }
    }
    Line::from(spans)
}

/// Whatever a prompt draws, no line may be wider than the panel.
fn finish(mut v: View, width: usize) -> View {
    v.body = v.body.into_iter().map(|l| clip(l, width)).collect();
    v.blocking()
}

pub enum Outcome {
    Stay,
    Decline,
    /// Every question answered: (question text, answer) in question order.
    Submit(Vec<(String, String)>),
}

/// Per-prompt interaction state.
#[derive(Default, Clone)]
pub struct Ui {
    /// Permission prompt: highlighted row.
    pub perm: usize,
    tab: usize,
    cursor: Vec<usize>,
    checked: Vec<Vec<bool>>,
    other: Vec<String>,
    editing: bool,
    done: Vec<Option<String>>,
    note: String,
    /// Permission prompt: what to do instead, typed on the "No" row and sent with the refusal.
    pub feedback: String,
}

fn options(q: &Value) -> &[Value] {
    q["options"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn label(o: &Value) -> &str {
    o["label"].as_str().unwrap_or_default()
}

fn is_multi(q: &Value) -> bool {
    q["multiSelect"].as_bool() == Some(true)
}

fn header(q: &Value, i: usize) -> String {
    q["header"].as_str().filter(|h| !h.is_empty()).map_or_else(|| format!("Q{}", i + 1), String::from)
}

impl Ui {
    pub fn for_questions(qs: &[Value]) -> Ui {
        let mut ui = Ui::default();
        ui.fit(qs);
        ui
    }

    fn fit(&mut self, qs: &[Value]) {
        self.cursor.resize(qs.len(), 0);
        self.other.resize(qs.len(), String::new());
        self.done.resize(qs.len(), None);
        self.checked.resize(qs.len(), vec![]);
        for (c, q) in self.checked.iter_mut().zip(qs) {
            c.resize(options(q).len(), false);
        }
        self.tab = self.tab.min(self.review_tab(qs).unwrap_or(0).max(qs.len().saturating_sub(1)));
    }

    /// Several questions end on a review tab; a single one is answered directly.
    fn review_tab(&self, qs: &[Value]) -> Option<usize> {
        (qs.len() > 1).then_some(qs.len())
    }

    // ---------- questions: keys ----------

    pub fn question_key(&mut self, qs: &[Value], k: KeyEvent) -> Outcome {
        if qs.is_empty() {
            return Outcome::Decline; // nothing to ask: do not leave the agent waiting
        }
        self.fit(qs);
        self.note.clear();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = qs.len();
        if self.review_tab(qs) == Some(self.tab) {
            return self.review_key(qs, k);
        }
        let i = self.tab;
        let (q, no) = (&qs[i], options(&qs[i]).len());
        let multi = is_multi(q);
        if self.editing {
            match k.code {
                KeyCode::Esc => self.editing = false,
                KeyCode::Enter if self.other[i].trim().is_empty() => self.editing = false,
                KeyCode::Enter => return self.confirm(qs, i),
                KeyCode::Backspace => {
                    self.other[i].pop();
                }
                KeyCode::Char('u') if ctrl => self.other[i].clear(),
                KeyCode::Char(c) if !ctrl => self.other[i].push(c),
                KeyCode::Up | KeyCode::Down => {
                    self.editing = false;
                    return self.question_key(qs, k);
                }
                _ => {}
            }
            return Outcome::Stay;
        }
        let rows = no + 1; // the options and "Other"
        match k.code {
            KeyCode::Esc => return Outcome::Decline,
            KeyCode::Up | KeyCode::Down => self.cursor[i] = crate::panel::cycle(self.cursor[i], rows, k.code == KeyCode::Up),
            KeyCode::Home => self.cursor[i] = 0,
            KeyCode::End => self.cursor[i] = no,
            KeyCode::Left | KeyCode::BackTab if n > 1 => self.tab = self.tab.saturating_sub(1),
            KeyCode::Right | KeyCode::Tab if n > 1 => self.tab = (self.tab + 1).min(n),
            KeyCode::Char(' ') if multi => self.toggle(i, no),
            KeyCode::Char(c @ '1'..='9') if !ctrl => {
                let idx = c as usize - '1' as usize;
                if idx < no {
                    self.cursor[i] = idx;
                    if multi {
                        self.checked[i][idx] = !self.checked[i][idx];
                    } else {
                        return self.confirm(qs, i);
                    }
                } else if idx == no {
                    self.cursor[i] = no;
                    self.editing = true;
                }
            }
            KeyCode::Enter => {
                let on_other = self.cursor[i] == no;
                if on_other && self.other[i].trim().is_empty() {
                    self.editing = true;
                } else if on_other || multi || !options(q).is_empty() {
                    return self.confirm(qs, i);
                }
            }
            // Any other printable key starts typing your own answer.
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                self.cursor[i] = no;
                self.editing = true;
                self.other[i].push(c);
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Pasted text goes into "Other" as text, never as hotkeys (a pasted "2 apples" must not pick option 2).
    pub fn question_paste(&mut self, qs: &[Value], text: &str) {
        self.fit(qs);
        let Some(i) = (self.tab < qs.len()).then_some(self.tab) else { return };
        self.cursor[i] = options(&qs[i]).len();
        self.editing = true;
        self.other[i].extend(text.chars().map(|c| if c.is_control() { ' ' } else { c }));
    }

    fn toggle(&mut self, i: usize, no: usize) {
        match self.cursor[i] {
            c if c < no => self.checked[i][c] = !self.checked[i][c],
            _ => self.editing = true,
        }
    }

    fn review_key(&mut self, qs: &[Value], k: KeyEvent) -> Outcome {
        let n = qs.len();
        match k.code {
            KeyCode::Esc => return Outcome::Decline,
            KeyCode::Left | KeyCode::BackTab => self.tab = n - 1,
            KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => self.tab = c as usize - '1' as usize,
            KeyCode::Enter => match self.done.iter().position(Option::is_none) {
                Some(first) => {
                    self.tab = first;
                    self.note = "this one still needs an answer".into();
                }
                None => {
                    return Outcome::Submit(qs.iter().zip(&self.done).map(|(q, a)| (q["question"].as_str().unwrap_or_default().to_string(), a.clone().unwrap_or_default())).collect());
                }
            },
            _ => {}
        }
        Outcome::Stay
    }

    /// Record the answer for question `i` from the cursor, checkboxes and "Other", then move on.
    fn confirm(&mut self, qs: &[Value], i: usize) -> Outcome {
        let (q, no) = (&qs[i], options(&qs[i]).len());
        let typed = self.other[i].trim().to_string();
        let answer = if is_multi(q) {
            let mut picked: Vec<String> = options(q).iter().enumerate().filter(|(j, _)| self.checked[i][*j]).map(|(_, o)| label(o).to_string()).collect();
            if !typed.is_empty() {
                picked.push(typed);
            }
            if picked.is_empty() {
                self.note = "pick at least one (space), or type your own".into();
                return Outcome::Stay;
            }
            picked.join(", ")
        } else if self.cursor[i] < no {
            label(&options(q)[self.cursor[i]]).to_string()
        } else {
            typed
        };
        self.editing = false;
        self.done[i] = Some(answer);
        let n = qs.len();
        if n == 1 {
            return Outcome::Submit(vec![(q["question"].as_str().unwrap_or_default().to_string(), self.done[0].clone().unwrap_or_default())]);
        }
        // next unanswered question after this one, else the review tab
        self.tab = (1..n).map(|d| (i + d) % n).find(|j| self.done[*j].is_none()).unwrap_or(n);
        Outcome::Stay
    }

    // ---------- questions: view ----------

    pub fn question_view(&self, qs: &[Value], width: usize) -> View {
        let n = qs.len();
        if n == 0 {
            return View::new("Question", vec![], 0, theme::hints(&[("esc", "decline")])).blocking();
        }
        let mut body: Vec<Line<'static>> = Vec::new();
        let mut focus = 0;
        if n > 1 {
            body.push(self.tab_line(qs));
            body.push(Line::raw(""));
        }
        if self.review_tab(qs) == Some(self.tab) {
            body.push(Line::styled("Your answers", theme::bold()));
            for (i, q) in qs.iter().enumerate() {
                let (mark, ans) = match &self.done[i] {
                    Some(a) => (Span::styled("✓ ", theme::ok()), a.clone()),
                    None => (Span::styled("✗ ", theme::danger()), "(not answered)".to_string()),
                };
                body.push(Line::from(vec![Span::raw(format!(" {}. ", i + 1)), mark, Span::styled(truncate(q["question"].as_str().unwrap_or_default(), width.saturating_sub(10)), theme::dim())]));
                body.push(Line::from(vec![Span::raw("      "), Span::styled(truncate(&ans, width.saturating_sub(8)), theme::bold())]));
            }
            if !self.note.is_empty() {
                body.push(Line::styled(self.note.clone(), theme::attention()));
            }
            let all = self.done.iter().all(Option::is_some);
            let footer = if all { theme::hints(&[("enter", "submit"), ("←", "back"), ("1-9", "edit one"), ("esc", "decline")]) } else { theme::hints(&[("enter", "go to the missing one"), ("←", "back"), ("esc", "decline")]) };
            return finish(View::new("Review", body, 0, footer), width);
        }
        let i = self.tab.min(n.saturating_sub(1));
        let q = &qs[i];
        let multi = is_multi(q);
        for l in wrap(q["question"].as_str().unwrap_or_default(), width) {
            body.push(Line::styled(l, theme::bold()));
        }
        let cursor = self.cursor.get(i).copied().unwrap_or(0);
        let no = options(q).len();
        let indent = 2 + if multi { 2 } else { 0 } + 3;
        for (j, o) in options(q).iter().enumerate() {
            let on = cursor == j;
            if on {
                focus = body.len();
            }
            let boxed = if multi { if self.checked.get(i).and_then(|c| c.get(j)) == Some(&true) { "☑ " } else { "☐ " } } else { "" };
            let num = if j < 9 { format!("{}. ", j + 1) } else { "   ".to_string() };
            let text = truncate(&format!("{boxed}{num}{}", label(o)), width.saturating_sub(2));
            body.push(row(on, vec![Span::raw(text)]));
            for d in wrap(o["description"].as_str().unwrap_or_default(), width.saturating_sub(indent)).into_iter().filter(|d| !d.is_empty()) {
                body.push(Line::styled(format!("{}{d}", " ".repeat(indent)), theme::dim()));
            }
        }
        // "Other": your own answer, typed inline
        let on = cursor == no;
        if on {
            focus = body.len();
        }
        let num = if no < 9 { format!("{}. ", no + 1) } else { "   ".to_string() };
        let typed = self.other.get(i).cloned().unwrap_or_default();
        if self.editing || !typed.is_empty() {
            let mut spans = vec![Span::raw(format!("✎ {num}Other: ")), Span::raw(truncate(&typed, width.saturating_sub(16)))];
            if self.editing {
                spans.push(Span::styled(" ", theme::selected())); // the text cursor
            }
            body.push(row_with(on, on && !self.editing, spans));
        } else {
            body.push(row(on, vec![Span::raw(format!("✎ {num}Other")), Span::styled(" — type your own answer", theme::dim())]));
        }
        // a preview of the highlighted option, when the question offers them
        if let Some(pv) = options(q).get(cursor).and_then(|o| o["preview"].as_str()).filter(|p| !p.trim().is_empty()) {
            body.push(Line::styled("─ preview ".to_string() + &"─".repeat(width.saturating_sub(11)), theme::dim()));
            for l in pv.lines().take(PREVIEW_LINES) {
                body.push(Line::styled(truncate(l, width), theme::code()));
            }
        }
        if !self.note.is_empty() {
            body.push(Line::styled(self.note.clone(), theme::attention()));
        }
        let mut keys: Vec<(&str, &str)> = Vec::new();
        if self.editing {
            keys.extend([("type", "your answer"), ("enter", "confirm"), ("esc", "stop typing")]);
        } else {
            keys.push(("↑↓", "move"));
            if multi {
                keys.extend([("space", "toggle"), ("enter", "confirm")]);
            } else {
                keys.push(("enter", "choose"));
            }
            keys.push(("1-9", if multi { "toggle" } else { "pick" }));
            if n > 1 {
                keys.push(("←→", "question"));
            }
            keys.push(("esc", "decline"));
        }
        let title = if n > 1 { format!("Question {}/{n}", i + 1) } else { format!("Question · {}", header(q, i)) };
        finish(View::new(title, body, focus, theme::hints(&keys)), width)
    }

    fn tab_line(&self, qs: &[Value]) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, q) in qs.iter().enumerate() {
            let done = self.done.get(i).is_some_and(Option::is_some);
            let label = format!(" {} {} ", if done { "✓" } else { "○" }, truncate(&header(q, i), 18));
            spans.push(Span::styled(label, if i == self.tab { theme::selected() } else if done { theme::ok() } else { theme::dim() }));
            spans.push(Span::raw(" "));
        }
        let review = self.tab == qs.len();
        spans.push(Span::styled(" Submit ", if review { theme::selected() } else { theme::dim() }));
        Line::from(spans)
    }

    // ---------- permissions ----------

    /// The "always" row says what it will remember, as Claude Code's own prompt does.
    pub fn always_label(suggestions: &Value) -> Option<String> {
        let parts: Vec<String> = suggestions
            .as_array()?
            .iter()
            .filter_map(|s| match s["type"].as_str()? {
                "setMode" => Some(match s["mode"].as_str()? {
                    "acceptEdits" => "auto-accept edits".to_string(),
                    m => format!("switch to {m} mode"),
                } + if s["destination"] == "session" { " for this session" } else { "" }),
                "addRules" => {
                    let rules: Vec<String> = s["rules"]
                        .as_array()?
                        .iter()
                        .filter_map(|r| {
                            let tool = r["toolName"].as_str()?;
                            Some(r["ruleContent"].as_str().map_or(tool.to_string(), |c| format!("{tool}({})", truncate(c, 48))))
                        })
                        .collect();
                    (!rules.is_empty()).then(|| format!("don't ask again for {}", rules.join(", ")))
                }
                "addDirectories" => {
                    let dirs: Vec<&str> = s["directories"].as_array()?.iter().filter_map(Value::as_str).collect();
                    (!dirs.is_empty()).then(|| format!("allow access to {}", dirs.join(", ")))
                }
                _ => None,
            })
            .collect();
        match (parts.is_empty(), suggestions.as_array().is_some_and(|a| !a.is_empty())) {
            (false, _) => Some(format!("Yes, and {}", parts.join(" and "))),
            (true, true) => Some("Yes, and don't ask again".into()),
            (true, false) => None,
        }
    }

    /// Rows of a permission prompt, as the choice each one makes.
    fn permission_rows(has_always: bool) -> Vec<(Choice, &'static str, &'static str)> {
        let mut rows = vec![(Choice::Allow, "Yes", "y")];
        if has_always {
            rows.push((Choice::Always, "Yes, and don't ask again", "a"));
        }
        rows.push((Choice::Deny, "No", "n"));
        rows
    }

    /// Pasted text is a reason for refusing, never an answer: it lands on the "No" row.
    pub fn paste_feedback(&mut self, text: &str, has_always: bool) {
        self.perm = Self::permission_rows(has_always).len() - 1;
        self.feedback.push_str(&text.replace(['\r', '\n'], " "));
    }

    pub fn permission_key(&mut self, has_always: bool, k: KeyEvent) -> Option<Choice> {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return None;
        }
        let rows = Self::permission_rows(has_always);
        self.perm = self.perm.min(rows.len() - 1);
        // On the "No" row the keyboard writes the reason, as in Claude Code; hotkeys work everywhere else.
        if self.perm == rows.len() - 1 {
            match k.code {
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::ALT) => {
                    self.feedback.push(c);
                    return None;
                }
                KeyCode::Backspace => {
                    self.feedback.pop();
                    return None;
                }
                _ => {}
            }
        }
        match k.code {
            KeyCode::Char('y' | 'Y') => Some(Choice::Allow),
            KeyCode::Char('a' | 'A') if has_always => Some(Choice::Always),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(Choice::Deny),
            KeyCode::Char(c @ '1'..='9') => rows.get(c as usize - '1' as usize).map(|r| r.0),
            KeyCode::Up | KeyCode::Down => {
                self.perm = crate::panel::cycle(self.perm, rows.len(), k.code == KeyCode::Up);
                None
            }
            KeyCode::Home => {
                self.perm = 0;
                None
            }
            KeyCode::End => {
                self.perm = rows.len() - 1;
                None
            }
            KeyCode::Enter => Some(rows[self.perm].0),
            _ => None,
        }
    }

    /// `always`: what the second row does, from `always_label`; `None` when claude offered nothing to remember.
    pub fn permission_view(&self, headline: &str, plan: Option<&str>, always: Option<&str>, width: usize) -> View {
        let mut body: Vec<Line<'static>> = Vec::new();
        for l in wrap(headline, width) {
            body.push(Line::styled(l, theme::bold()));
        }
        if let Some(plan) = plan {
            let lines = crate::markdown::render(plan, width);
            let n = lines.len();
            body.extend(lines.into_iter().take(30));
            if n > 30 {
                body.push(Line::styled(format!("… +{} more lines", n - 30), theme::dim()));
            }
            body.push(Line::raw(""));
        }
        let rows = Self::permission_rows(always.is_some());
        let cur = self.perm.min(rows.len() - 1);
        let mut focus = 0;
        for (i, (choice, text, key)) in rows.iter().enumerate() {
            let on = i == cur;
            if on {
                focus = body.len();
            }
            // (words, hint after them): the "No" row takes typed words saying what to do instead.
            let (text, hint, caret) = match choice {
                Choice::Always => (always.unwrap_or(text).to_string(), String::new(), false),
                Choice::Deny if !self.feedback.is_empty() => (format!("No: {}", self.feedback), String::new(), on),
                Choice::Deny if on => ("No,".to_string(), " type what to do instead, or enter to just refuse".to_string(), true),
                _ => (text.to_string(), String::new(), false),
            };
            // A long choice (an "always" naming a folder, a long reason) wraps under itself.
            let num = format!("{}. ", i + 1);
            let tail = if caret { 1 } else { key.width() + 4 };
            let room = width.saturating_sub(2 + num.width() + tail).max(8);
            let parts = wrap(&format!("{text}{hint}"), room);
            let last = parts.len() - 1;
            // The hint keeps its dim style across the wrap: count off the words' characters line by line.
            let mut words_left = text.chars().count();
            for (j, part) in parts.into_iter().enumerate() {
                let lead = if j == 0 { num.clone() } else { " ".repeat(num.width()) };
                let n = words_left.min(part.chars().count());
                words_left -= n;
                let mine: String = part.chars().take(n).collect();
                let rest: String = part.chars().skip(n).collect();
                let mut spans = vec![Span::raw(format!("{lead}{mine}")), Span::styled(rest, theme::dim())];
                if j == last {
                    spans.push(if caret { Span::styled("▏", theme::accent()) } else { Span::styled(format!("  ({key})"), theme::dim()) });
                }
                body.push(if j == 0 { row(on, spans) } else { row_with(false, on, spans) });
            }
        }
        let keys = if always.is_some() { "y/a/n" } else { "y/n" };
        let footer = theme::hints(&[("↑↓", "move"), ("enter", "choose"), (keys, "answer at once"), ("esc", "deny")]);
        finish(View::new("Permission", body, focus, footer), width)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn line_width(l: &Line) -> usize {
        l.spans.iter().map(|s| s.content.width()).sum()
    }
    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn press(ui: &mut Ui, qs: &[Value], c: KeyCode) -> Outcome {
        ui.question_key(qs, key(c))
    }
    fn typed(ui: &mut Ui, qs: &[Value], s: &str) {
        s.chars().for_each(|c| {
            press(ui, qs, KeyCode::Char(c));
        });
    }
    fn text(v: &View) -> Vec<String> {
        v.body.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }
    fn single() -> Vec<Value> {
        vec![json!({"question": "Which approach?", "header": "Approach", "multiSelect": false, "options": [
            {"label": "Fast", "description": "quick and dirty"}, {"label": "Careful", "description": "slow and right"}]})]
    }
    fn two() -> Vec<Value> {
        vec![
            single()[0].clone(),
            json!({"question": "Which checks?", "header": "Checks", "multiSelect": true, "options": [
                {"label": "Lint"}, {"label": "Tests"}, {"label": "Types"}]}),
        ]
    }
    fn submitted(o: Outcome) -> Vec<(String, String)> {
        match o {
            Outcome::Submit(a) => a,
            Outcome::Stay => panic!("stayed"),
            Outcome::Decline => panic!("declined"),
        }
    }

    #[test]
    fn single_choice_moves_wraps_and_confirms_with_enter_or_a_digit() {
        let qs = single();
        let mut ui = Ui::for_questions(&qs);
        assert!(matches!(press(&mut ui, &qs, KeyCode::Down), Outcome::Stay));
        assert_eq!(submitted(press(&mut ui, &qs, KeyCode::Enter)), [("Which approach?".to_string(), "Careful".to_string())]);

        let mut ui = Ui::for_questions(&qs);
        press(&mut ui, &qs, KeyCode::Up); // wraps to "Other"
        assert_eq!(ui.cursor[0], 2);
        press(&mut ui, &qs, KeyCode::Down);
        assert_eq!(ui.cursor[0], 0, "and wraps back");
        assert_eq!(submitted(press(&mut ui, &qs, KeyCode::Char('1')))[0].1, "Fast", "a digit picks and confirms at once");
        assert!(matches!(press(&mut Ui::for_questions(&qs), &qs, KeyCode::Esc), Outcome::Decline));
    }

    #[test]
    fn other_is_typed_inline_by_a_digit_enter_or_just_typing() {
        let qs = single();
        // typing anywhere starts your own answer
        let mut ui = Ui::for_questions(&qs);
        typed(&mut ui, &qs, "use X");
        assert!(ui.editing && ui.other[0] == "use X" && ui.cursor[0] == 2);
        press(&mut ui, &qs, KeyCode::Backspace);
        assert_eq!(submitted(press(&mut ui, &qs, KeyCode::Enter))[0].1, "use", "typed answers are trimmed");
        // the digit after the last option goes to Other; digits are then text
        let mut ui = Ui::for_questions(&qs);
        press(&mut ui, &qs, KeyCode::Char('3'));
        typed(&mut ui, &qs, "2 apples");
        assert_eq!(ui.other[0], "2 apples");
        // Esc stops typing but keeps the text; a second Esc declines
        press(&mut ui, &qs, KeyCode::Esc);
        assert!(!ui.editing && ui.other[0] == "2 apples");
        assert!(matches!(press(&mut ui, &qs, KeyCode::Esc), Outcome::Decline));
        // Enter on a non-empty Other confirms it; on an empty one it starts typing
        let mut ui = Ui::for_questions(&qs);
        press(&mut ui, &qs, KeyCode::End);
        assert!(matches!(press(&mut ui, &qs, KeyCode::Enter), Outcome::Stay) && ui.editing);
        assert!(matches!(press(&mut ui, &qs, KeyCode::Enter), Outcome::Stay) && !ui.editing, "empty text: stop typing, nothing sent");
        // up/down leave typing and move
        let mut ui = Ui::for_questions(&qs);
        typed(&mut ui, &qs, "ab");
        press(&mut ui, &qs, KeyCode::Up);
        assert!(!ui.editing && ui.cursor[0] == 1);
    }

    #[test]
    fn multi_select_toggles_and_needs_at_least_one() {
        let qs = vec![two()[1].clone()];
        let mut ui = Ui::for_questions(&qs);
        assert!(matches!(press(&mut ui, &qs, KeyCode::Enter), Outcome::Stay));
        assert!(!ui.note.is_empty(), "explains why nothing happened");
        press(&mut ui, &qs, KeyCode::Char('1'));
        press(&mut ui, &qs, KeyCode::Down);
        press(&mut ui, &qs, KeyCode::Down);
        press(&mut ui, &qs, KeyCode::Char(' '));
        press(&mut ui, &qs, KeyCode::Char('3')); // toggles Types off again
        press(&mut ui, &qs, KeyCode::Char('3'));
        assert_eq!(ui.checked[0], [true, false, true]);
        assert_eq!(submitted(press(&mut ui, &qs, KeyCode::Enter))[0].1, "Lint, Types");
        // typed text joins the checked options
        let mut ui = Ui::for_questions(&qs);
        press(&mut ui, &qs, KeyCode::Char('2'));
        typed(&mut ui, &qs, "docs");
        assert_eq!(submitted(press(&mut ui, &qs, KeyCode::Enter))[0].1, "Tests, docs");
    }

    #[test]
    fn several_questions_advance_review_and_submit_in_order() {
        let qs = two();
        let mut ui = Ui::for_questions(&qs);
        assert!(matches!(press(&mut ui, &qs, KeyCode::Char('2')), Outcome::Stay));
        assert_eq!(ui.tab, 1, "answering moves to the next question");
        press(&mut ui, &qs, KeyCode::Char('1'));
        press(&mut ui, &qs, KeyCode::Char('2'));
        assert!(matches!(press(&mut ui, &qs, KeyCode::Enter), Outcome::Stay));
        assert_eq!(ui.tab, 2, "all answered: the review tab");
        // go back and change the first answer, then submit
        press(&mut ui, &qs, KeyCode::Char('1'));
        assert_eq!(ui.tab, 0);
        assert_eq!(ui.cursor[0], 1, "your earlier choice is still highlighted");
        press(&mut ui, &qs, KeyCode::Char('1'));
        assert_eq!(ui.tab, 2, "the next unanswered... none left: review");
        let answers = submitted(press(&mut ui, &qs, KeyCode::Enter));
        assert_eq!(answers, [("Which approach?".to_string(), "Fast".to_string()), ("Which checks?".to_string(), "Lint, Tests".to_string())]);
    }

    #[test]
    fn review_sends_you_to_what_is_missing_and_arrows_switch_tabs() {
        let qs = two();
        let mut ui = Ui::for_questions(&qs);
        press(&mut ui, &qs, KeyCode::Right);
        press(&mut ui, &qs, KeyCode::Right);
        assert_eq!(ui.tab, 2);
        assert!(matches!(press(&mut ui, &qs, KeyCode::Enter), Outcome::Stay));
        assert_eq!(ui.tab, 0, "unanswered: jumps to the first missing question");
        assert!(matches!(press(&mut ui, &qs, KeyCode::Right), Outcome::Stay));
        assert!(matches!(press(&mut ui, &qs, KeyCode::Left), Outcome::Stay));
        assert_eq!(ui.tab, 0);
        press(&mut ui, &qs, KeyCode::Left);
        assert_eq!(ui.tab, 0, "no wrap on tabs");
        press(&mut ui, &qs, KeyCode::Right);
        press(&mut ui, &qs, KeyCode::Right);
        assert!(matches!(press(&mut ui, &qs, KeyCode::Esc), Outcome::Decline));
    }

    #[test]
    fn the_view_shows_cursor_checkboxes_descriptions_other_and_hints() {
        let qs = two();
        let mut ui = Ui::for_questions(&qs);
        let v = ui.question_view(&qs, 60);
        let t = text(&v);
        assert!(t[0].contains("Approach") && t[0].contains("Checks") && t[0].contains("Submit"), "tabs: {:?}", t[0]);
        assert_eq!(v.title, "Question 1/2");
        assert!(t.iter().any(|l| l.starts_with("❯ 1. Fast")));
        assert!(t.iter().any(|l| l.trim() == "quick and dirty"));
        assert!(t.iter().any(|l| l.contains("✎ 3. Other — type your own answer")));
        assert_eq!(v.body[v.focus].spans[1].content, "1. Fast");
        press(&mut ui, &qs, KeyCode::Char('1'));
        let v = ui.question_view(&qs, 60);
        let t = text(&v);
        assert!(t.iter().any(|l| l.contains("☐ 1. Lint")), "multi-select shows checkboxes");
        press(&mut ui, &qs, KeyCode::Char('1'));
        assert!(text(&ui.question_view(&qs, 60)).iter().any(|l| l.contains("☑ 1. Lint")));
        let footer: String = ui.question_view(&qs, 60).footer.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(footer.contains("␣ toggle") && footer.contains("←→ question") && footer.contains("⎋ decline"), "{footer}");
        // typing shows the text with the cursor, and the hints change
        typed(&mut ui, &qs, "hey");
        let v = ui.question_view(&qs, 60);
        assert!(text(&v).iter().any(|l| l.contains("✎ 4. Other: hey")));
        let footer: String = v.footer.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(footer.contains("⎋ stop typing"), "{footer}");
    }

    #[test]
    fn previews_follow_the_highlighted_option() {
        let qs = vec![json!({"question": "Layout?", "header": "Layout", "multiSelect": false, "options": [
            {"label": "Wide", "preview": "+------+\n| wide |\n+------+"}, {"label": "Tall", "preview": "|t|\n|a|"}, {"label": "None"}]})];
        let mut ui = Ui::for_questions(&qs);
        assert!(text(&ui.question_view(&qs, 40)).iter().any(|l| l.contains("| wide |")));
        press(&mut ui, &qs, KeyCode::Down);
        let t = text(&ui.question_view(&qs, 40));
        assert!(t.iter().any(|l| l.contains("|a|")) && !t.iter().any(|l| l.contains("| wide |")));
        press(&mut ui, &qs, KeyCode::Down);
        assert!(!text(&ui.question_view(&qs, 40)).iter().any(|l| l.contains("preview")), "no preview, no box");
    }

    #[test]
    fn nothing_is_wider_than_the_panel_and_odd_input_does_not_panic() {
        let long = "x".repeat(300);
        let qs = vec![json!({"question": long, "header": "H", "multiSelect": true, "options": [
            {"label": long, "description": long, "preview": long}, {"label": ""}, {}]})];
        let mut ui = Ui::for_questions(&qs);
        for w in [8, 20, 60] {
            for l in &ui.question_view(&qs, w).body {
                assert!(line_width(l) <= w, "width {w}: {}", line_width(l));
            }
        }
        for c in [KeyCode::Down, KeyCode::Char(' '), KeyCode::Char('9'), KeyCode::Enter, KeyCode::Tab, KeyCode::Left, KeyCode::End, KeyCode::F(1)] {
            press(&mut ui, &qs, c);
        }
        // no options at all, and no questions at all
        let empty = vec![json!({"question": "?"})];
        let mut ui = Ui::for_questions(&empty);
        press(&mut ui, &empty, KeyCode::Enter);
        assert!(matches!(press(&mut ui, &empty, KeyCode::Char('x')), Outcome::Stay));
        let _ = ui.question_view(&[], 20);
    }

    #[test]
    fn pasted_text_is_text_not_hotkeys() {
        let qs = single();
        let mut ui = Ui::for_questions(&qs);
        ui.question_paste(&qs, "2 apples\nplease");
        assert!(ui.editing && ui.other[0] == "2 apples please" && ui.cursor[0] == 2);
        assert_eq!(submitted(press(&mut ui, &qs, KeyCode::Enter))[0].1, "2 apples please");
    }

    #[test]
    fn permission_prompt_has_the_same_grammar() {
        let mut ui = Ui::default();
        let k = |c| key(c);
        assert!(ui.permission_key(true, k(KeyCode::Down)).is_none());
        assert!(matches!(ui.permission_key(true, k(KeyCode::Enter)), Some(Choice::Always)), "second row is 'don't ask again'");
        assert!(matches!(ui.permission_key(true, k(KeyCode::Char('1'))), Some(Choice::Allow)));
        assert!(matches!(ui.permission_key(true, k(KeyCode::Char('3'))), Some(Choice::Deny)));
        assert!(matches!(ui.permission_key(true, k(KeyCode::Char('y'))), Some(Choice::Allow)));
        assert!(matches!(ui.permission_key(true, k(KeyCode::Esc)), Some(Choice::Deny)));
        // without suggestions there is no "always" row: 'a' and 2 do not mean it
        let mut ui = Ui::default();
        assert!(ui.permission_key(false, k(KeyCode::Char('a'))).is_none());
        assert!(matches!(ui.permission_key(false, k(KeyCode::Char('2'))), Some(Choice::Deny)));
        assert!(ui.permission_key(false, k(KeyCode::Char('3'))).is_none());
        ui.permission_key(false, k(KeyCode::Up));
        assert!(matches!(ui.permission_key(false, k(KeyCode::Enter)), Some(Choice::Deny)), "up from the top wraps to No");
        assert!(ui.permission_key(true, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)).is_none(), "ctrl keys belong to the app");
        let v = Ui::default().permission_view("Allow Bash(ls)?", Some("plan line"), Some("Yes, and don't ask again"), 50);
        let t = text(&v);
        assert_eq!((t[0].as_str(), t[1].as_str()), ("Allow Bash(ls)?", "plan line"));
        assert!(t[2].is_empty(), "a blank line sets the choices apart from what is shown");
        assert!(t[3].starts_with("❯ 1. Yes") && t[4].contains("2. Yes, and don't ask again") && t[5].contains("3. No"));
        assert_eq!(v.title, "Permission");
    }

    #[test]
    fn the_no_row_takes_a_reason_and_hotkeys_still_work_elsewhere() {
        let mut ui = Ui::default();
        assert!(matches!(ui.permission_key(false, key(KeyCode::Char('n'))), Some(Choice::Deny)), "n refuses from the Yes row");
        ui.permission_key(false, key(KeyCode::Down));
        for c in "use yarn".chars() {
            assert!(ui.permission_key(false, key(KeyCode::Char(c))).is_none(), "letters on the No row are text, not hotkeys");
        }
        ui.permission_key(false, key(KeyCode::Backspace));
        assert_eq!(ui.feedback, "use yar");
        assert!(matches!(ui.permission_key(false, key(KeyCode::Enter)), Some(Choice::Deny)));
        assert!(text(&ui.permission_view("Allow Bash(ls)?", None, None, 60)).iter().any(|l| l.contains("No: use yar")));
    }

    #[test]
    fn a_long_choice_wraps_inside_the_panel_and_keeps_its_key() {
        let ui = Ui::default();
        let always = "Yes, and allow access to /private/tmp/a/very/long/folder/name/for/this/project from this project";
        let v = ui.permission_view("Run this command?", None, Some(always), 40);
        let rows = text(&v);
        assert!(v.body.iter().all(|l| line_width(l) <= 40), "{rows:?}");
        let at = rows.iter().position(|r| r.contains("2. Yes")).unwrap();
        let end = rows.iter().position(|r| r.ends_with("(a)")).unwrap();
        assert!(end > at && rows[at + 1].starts_with("     "), "continued under itself: {rows:?}");
    }

    #[test]
    fn a_long_reason_on_the_no_row_wraps_and_keeps_the_caret_in_view() {
        let mut ui = Ui::default();
        ui.permission_key(false, key(KeyCode::Down));
        let v = ui.permission_view("Allow Bash(ls)?", None, None, 30);
        assert!(v.body.iter().all(|l| line_width(l) <= 30), "the hint wraps: {:?}", text(&v));
        ui.paste_feedback("please use the other folder and run the tests first", false);
        let v = ui.permission_view("Allow Bash(ls)?", None, None, 30);
        let rows = text(&v);
        assert!(v.body.iter().all(|l| line_width(l) <= 30), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("No: please")) && rows.last().unwrap().ends_with("first▏"), "{rows:?}");
    }

    #[test]
    fn the_always_row_names_what_it_remembers() {
        let l = |v: Value| Ui::always_label(&v);
        assert_eq!(l(json!([{"type":"setMode","mode":"acceptEdits","destination":"session"}])).unwrap(), "Yes, and auto-accept edits for this session");
        assert_eq!(
            l(json!([{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"npm test:*"}],"behavior":"allow","destination":"localSettings"}])).unwrap(),
            "Yes, and don't ask again for Bash(npm test:*)"
        );
        assert_eq!(l(json!([{"type":"somethingNew"}])).unwrap(), "Yes, and don't ask again", "unknown kinds keep the generic label");
        assert_eq!(l(json!([])), None, "nothing to remember, no row");
    }
}
