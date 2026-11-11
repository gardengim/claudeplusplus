//! Plan limits: the five-hour and seven-day windows Claude Code reports. `/config` → Rate limits chooses
//! a level at which claude++ warns, and a level at which it holds new messages until the window resets.
//! The windows belong to the account, so every tab's latest report counts, not only the one in front.

use super::*;

/// One plan window that has not reset yet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Window {
    pub name: &'static str,
    /// Percent used, 0–100.
    pub pct: f64,
    /// When it resets, seconds since the epoch.
    pub reset: f64,
}

impl Window {
    pub(crate) fn resets_at(&self) -> SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs_f64(self.reset.max(0.0))
    }
    pub(crate) fn describe(&self) -> String {
        format!("{} limit at {:.0}% (resets {})", self.name, self.pct, queue::clock(self.resets_at()))
    }
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

impl App {
    /// The open windows, each from the newest report of any tab (the later reset wins, then the higher use).
    pub(crate) fn limit_windows(&self) -> Vec<Window> {
        let now = now_secs();
        let mut out: Vec<Window> = vec![];
        for s in &self.sessions {
            for (name, w) in [("5-hour", s.five_hour), ("7-day", s.seven_day)] {
                let Some((pct, reset)) = w.filter(|w| w.1 > now) else { continue };
                match out.iter_mut().find(|o| o.name == name) {
                    Some(o) if (reset, pct) > (o.reset, o.pct) => (o.pct, o.reset) = (pct, reset),
                    Some(_) => {}
                    None => out.push(Window { name, pct, reset }),
                }
            }
        }
        out
    }

    /// The window that holds new messages right now: at or over the hold level, and not overridden by sending now.
    pub(crate) fn limit_block(&self) -> Option<Window> {
        if self.limit_hold == 0 {
            return None;
        }
        let hit = self.limit_windows().into_iter().filter(|w| w.pct >= f64::from(self.limit_hold)).max_by(|a, b| a.reset.total_cmp(&b.reset))?;
        (self.limit_released != Some(hit.reset as i64)).then_some(hit)
    }

    /// "Send now" on a held message: this window no longer holds anything.
    pub(crate) fn release_limit(&mut self) {
        self.limit_released = self.limit_windows().iter().map(|w| w.reset as i64).max();
    }

    /// A plain message while a window is over the hold level goes to the queue, due when the window resets.
    pub(crate) fn hold_for_limit(&mut self, text: String, w: Window) {
        self.queue_message(text, Some(w.resets_at()));
        self.s().note(format!("{}: held until it resets. alt+q → send now to go ahead anyway", w.describe()));
    }

    /// Once per window and reset: say so when a window first reaches the warning level.
    pub(crate) fn warn_on_limits(&mut self) {
        if self.limit_warn == 0 {
            return;
        }
        for w in self.limit_windows() {
            if w.pct < f64::from(self.limit_warn) || self.limit_warned.iter().any(|(n, r)| *n == w.name && *r == w.reset as i64) {
                continue;
            }
            self.limit_warned.retain(|(n, _)| *n != w.name);
            self.limit_warned.push((w.name, w.reset as i64));
            let text = w.describe();
            self.s().note(text.clone());
            self.flash(text.clone());
            self.notify(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_names_its_use_and_when_it_resets() {
        let w = Window { name: "5-hour", pct: 92.4, reset: 4_000_000_000.0 };
        let text = w.describe();
        assert!(text.starts_with("5-hour limit at 92% (resets ") && text.ends_with(')'), "{text}");
    }
}
