//! Search while you type (ctrl+f, `/search`, `/` in vim normal mode): this conversation's messages, tool calls
//! and replies, or the messages you have sent before. The conversation scrolls to the highlighted match as you
//! move; Enter stays there, Esc goes back. A message from history goes into the box.

use crate::panel::{self, View, cycle};
use crate::session::Item;
use crate::theme;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::{Line, Span};

const MAX_HITS: usize = 200;

pub struct Hit {
    /// Index into the conversation's items, or into the history.
    pub at: usize,
    /// The snippet: before the match, the match, after it.
    pub text: (String, String, String),
    pub who: &'static str,
}

#[derive(Default)]
pub struct Ui {
    pub query: String,
    pub cursor: usize,
    /// Searching what you sent before, not this conversation.
    pub history: bool,
    pub hits: Vec<Hit>,
    /// Where the conversation was scrolled before the search, to go back there on Esc.
    pub scroll: usize,
}

pub enum Outcome {
    Stay,
    Close,
    /// Keep the view where the search moved it.
    Go,
    /// Put this history entry in the box.
    Recall(usize),
}

/// `text` around the first of `words` (already lowercase) it contains; `None` when it lacks any of them.
fn snippet(text: &str, words: &[Vec<char>]) -> Option<(String, String, String)> {
    let chars: Vec<char> = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let low: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let find = |w: &[char]| (!w.is_empty()).then(|| low.windows(w.len()).position(|x| x == w)).flatten();
    let mut first = usize::MAX;
    let mut len = 0;
    for w in words {
        let p = find(w)?;
        if p < first {
            (first, len) = (p, w.len());
        }
    }
    if words.is_empty() {
        return None;
    }
    let from = first.saturating_sub(28);
    let s = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    let after = (first + len + 60).min(chars.len());
    Some((format!("{}{}", if from > 0 { "…" } else { "" }, s(from, first)), s(first, first + len), format!("{}{}", s(first + len, after), if after < chars.len() { "…" } else { "" })))
}

impl Ui {
    pub fn new(scroll: usize) -> Ui {
        Ui { scroll, ..Default::default() }
    }

    pub fn refresh(&mut self, items: &[Item], history: &[String]) {
        let words: Vec<Vec<char>> = self.query.to_lowercase().split_whitespace().map(|w| w.chars().collect()).collect();
        let mut hits = Vec::new();
        if self.history {
            for (i, h) in history.iter().enumerate().rev() {
                // Nothing typed yet: the latest ones, as ctrl+r in Claude Code starts.
                let text = if words.is_empty() { Some((String::new(), String::new(), h.replace('\n', " ").chars().take(90).collect())) } else { snippet(h, &words) };
                if let Some(text) = text {
                    hits.push(Hit { at: i, text, who: "·" });
                }
                if hits.len() >= MAX_HITS {
                    break;
                }
            }
        } else {
            for (i, it) in items.iter().enumerate().rev() {
                use crate::session::Entry;
                let who = match it.entry {
                    Entry::User(_) | Entry::Rich(_) => "❯",
                    Entry::Assistant { .. } => "●",
                    Entry::Tool { .. } => "$",
                    Entry::Error(_) => "✗",
                    _ => " ",
                };
                if let Some(text) = snippet(&it.entry.text(), &words) {
                    hits.push(Hit { at: i, text, who });
                }
                if hits.len() >= MAX_HITS {
                    break;
                }
            }
        }
        self.hits = hits;
        self.cursor = self.cursor.min(self.hits.len().saturating_sub(1));
    }

    pub fn key(&mut self, k: KeyEvent, items: &[Item], history: &[String]) -> Outcome {
        let (ctrl, alt) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT));
        let n = self.hits.len();
        match k.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('c' | 'f') if ctrl => return Outcome::Close,
            KeyCode::Enter => {
                return match self.hits.get(self.cursor) {
                    Some(h) if self.history => Outcome::Recall(h.at),
                    Some(_) => Outcome::Go,
                    None => Outcome::Close,
                };
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.history = !self.history;
                self.cursor = 0;
                self.refresh(items, history);
            }
            KeyCode::Up if n > 0 => self.cursor = cycle(self.cursor, n, true),
            KeyCode::Down if n > 0 => self.cursor = cycle(self.cursor, n, false),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(8),
            KeyCode::PageDown => self.cursor = (self.cursor + 8).min(n.saturating_sub(1)),
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.refresh(items, history);
            }
            KeyCode::Char('w') if ctrl => {
                let t = self.query.trim_end().len();
                self.query.truncate(self.query[..t].rfind(' ').map_or(0, |i| i + 1));
                self.refresh(items, history);
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refresh(items, history);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.query.push(c);
                self.cursor = 0;
                self.refresh(items, history);
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// The item to show the conversation at, for the highlighted match.
    pub fn reveal(&self) -> Option<usize> {
        self.hits.get(self.cursor).filter(|_| !self.history).map(|h| h.at)
    }
}

pub fn view(ui: &Ui) -> View {
    let chip = |label: &str, on: bool| Span::styled(format!(" {label} "), if on { theme::selected() } else { theme::dim() });
    let head = vec![Line::from(vec![
        Span::styled("/ ", theme::accent()),
        Span::raw(ui.query.clone()),
        Span::styled("▏", theme::accent()),
        Span::raw("   "),
        chip("this conversation", !ui.history),
        chip("messages you sent", ui.history),
    ])];
    let body: Vec<Line<'static>> = if ui.hits.is_empty() {
        vec![Line::styled(if ui.query.trim().is_empty() { "type to search" } else { "no match" }, theme::dim())]
    } else {
        ui.hits
            .iter()
            .enumerate()
            .map(|(i, h)| {
                panel::row(i == ui.cursor, vec![Span::styled(format!("{} ", h.who), theme::dim()), Span::raw(h.text.0.clone()), Span::styled(h.text.1.clone(), theme::accent().add_modifier(ratatui::style::Modifier::BOLD)), Span::raw(h.text.2.clone())])
            })
            .collect()
    };
    let n = ui.hits.len();
    let title = if n >= MAX_HITS { format!("Search · first {n}, newest first") } else { format!("Search · {n} match{}", if n == 1 { "" } else { "es" }) };
    let footer = theme::hints(&[("type", "search"), ("↑↓", "match"), ("tab", "conversation / history"), ("enter", if ui.history { "use" } else { "stay here" }), ("esc", "back")]);
    View::new(title, body, ui.cursor, footer).with_head(head).rows(5, 10)
}
