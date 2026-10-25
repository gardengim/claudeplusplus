//! `/usage`: where this tab's context goes and how much of your plan's limits are used. Both come from Claude
//! Code itself (`get_context_usage` and `get_usage` control requests), so nothing here costs tokens.
//! Only what the panel shows is kept.

use crate::panel::View;
use crate::theme;
use crate::ui::wrap;
use ratatui::text::{Line, Span};
use serde_json::Value;

#[derive(Default, Clone)]
pub struct Usage {
    pub ctx: Option<Ctx>,
    pub plan: Vec<Limit>,
    /// Both replies are in (or the backend does not know the requests).
    pub asked: bool,
}

#[derive(Clone)]
pub struct Ctx {
    pub total: u64,
    pub max: u64,
    pub model: String,
    /// (name, tokens, kind: used | deferred | buffer | free)
    pub cats: Vec<(String, u64, String)>,
    pub tool_calls: u64,
    pub tool_results: u64,
    pub attachments: u64,
    pub skills: (u64, u64),
    /// (name, tokens) of each skill in the listing.
    pub skill_list: Vec<(String, u64)>,
}

#[derive(Clone)]
pub struct Limit {
    pub name: String,
    pub percent: f64,
    pub resets_at: f64,
}

/// Seconds since the epoch of an ISO-8601 UTC timestamp like `2026-10-24T20:40:00.055509+00:00`.
pub fn iso_epoch(s: &str) -> Option<f64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split(['+', 'Z']).next()?;
    let mut t = time.split(':');
    let (h, mi) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let sec = t.next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60) as f64 + sec)
}

pub fn parse_context(r: &Value) -> Option<Ctx> {
    let cats = r["categories"].as_array()?;
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let mb = &r["messageBreakdown"];
    Some(Ctx {
        total: n(&r["totalTokens"]),
        max: n(&r["maxTokens"]),
        model: r["model"].as_str().unwrap_or_default().into(),
        cats: cats.iter().map(|c| (c["name"].as_str().unwrap_or("?").to_string(), n(&c["tokens"]), c["kind"].as_str().unwrap_or("used").to_string())).collect(),
        tool_calls: n(&mb["toolCallTokens"]),
        tool_results: n(&mb["toolResultTokens"]),
        attachments: n(&mb["attachmentTokens"]),
        skills: (n(&r["skills"]["totalSkills"]), n(&r["skills"]["tokens"])),
        skill_list: r["skills"]["skillFrontmatter"].as_array().into_iter().flatten().filter_map(|k| Some((k["name"].as_str()?.to_string(), n(&k["tokens"])))).collect(),
    })
}

pub fn parse_plan(r: &Value) -> Vec<Limit> {
    let rl = &r["rate_limits"];
    [("5-hour", "five_hour"), ("weekly", "seven_day"), ("weekly Opus", "seven_day_opus"), ("weekly Sonnet", "seven_day_sonnet")]
        .iter()
        .filter_map(|(name, key)| {
            let l = &rl[*key];
            Some(Limit { name: name.to_string(), percent: l["utilization"].as_f64()?, resets_at: l["resets_at"].as_str().and_then(iso_epoch).unwrap_or(0.0) })
        })
        .collect()
}

pub fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

pub fn until(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

fn bar(frac: f64, width: usize) -> String {
    let full = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(full), "░".repeat(width - full))
}

pub fn view(u: &Usage, width: usize, now: f64) -> View {
    let mut body: Vec<Line> = vec![];
    for l in &u.plan {
        let style = if l.percent >= 90.0 { theme::danger() } else if l.percent >= 70.0 { theme::attention() } else { theme::accent() };
        let reset = if l.resets_at > 0.0 { format!("  resets in {}", until(l.resets_at - now)) } else { String::new() };
        body.push(Line::from(vec![Span::styled(format!("{:<14}", l.name), theme::bold()), Span::styled(bar(l.percent / 100.0, 20), style), Span::raw(format!(" {:>3.0}%", l.percent)), Span::styled(reset, theme::dim())]));
    }
    let mut hint = String::new();
    match &u.ctx {
        None => body.push(Line::styled(if u.asked { "no context data from this Claude Code" } else { "asking Claude Code…" }, theme::dim())),
        Some(c) => {
            if !body.is_empty() {
                body.push(Line::raw(""));
            }
            body.push(Line::from(vec![
                Span::styled("Context        ", theme::bold()),
                Span::raw(format!("{} of {} used ({:.0}%)", tokens(c.total), tokens(c.max), c.total as f64 * 100.0 / c.max.max(1) as f64)),
                Span::styled(format!("  {}", c.model), theme::dim()),
            ]));
            let mut cats: Vec<&(String, u64, String)> = c.cats.iter().filter(|x| x.1 > 0 && x.2 != "free").collect();
            cats.sort_by_key(|x| (x.2 == "buffer", x.2 == "deferred", std::cmp::Reverse(x.1)));
            let top = cats.iter().map(|x| x.1).max().unwrap_or(1).max(1);
            let used = c.total.max(1);
            for (name, t, kind) in cats {
                let dim = kind != "used";
                let share = if dim { String::new() } else { format!(" {:>3.0}%", *t as f64 * 100.0 / used as f64) };
                let label = match kind.as_str() {
                    "deferred" => format!("{} (on demand)", name.trim_end_matches(" (deferred)")),
                    "buffer" => format!("{name} (reserved)"),
                    _ => name.clone(),
                };
                let bar_style = if dim { theme::dim() } else { theme::accent() };
                body.push(Line::from(vec![
                    Span::styled(format!("  {label:<34}"), if dim { theme::dim() } else { Default::default() }),
                    Span::styled(format!("{:>7} ", tokens(*t)), if dim { theme::dim() } else { theme::bold() }),
                    Span::styled(bar(*t as f64 / top as f64, 16), bar_style),
                    Span::styled(share, theme::dim()),
                ]));
            }
            if c.tool_results + c.tool_calls + c.attachments > 0 {
                body.push(Line::styled(format!("  messages: tool results {} · tool calls {} · attachments (hook output, @files) {}", tokens(c.tool_results), tokens(c.tool_calls), tokens(c.attachments)), theme::dim()));
            }
            if c.skills.1 * 100 >= used * 15 && c.skills.1 > 2000 {
                hint = format!("Skills take {} tokens ({} skills, {:.0}% of the context) in every request. /profile sources (or lean) skips your user-level skills; /new sources opens such a tab.", tokens(c.skills.1), c.skills.0, c.skills.1 as f64 * 100.0 / used as f64);
            }
        }
    }
    let mut tail = vec![Line::raw("")];
    tail.extend(wrap(&hint, width.saturating_sub(4).max(20)).into_iter().filter(|l| !l.is_empty()).map(|l| Line::styled(l, theme::dim())));
    View::new("Usage", body, 0, theme::hints(&[("r", "refresh"), ("esc", "close")])).with_tail(tail).rows(0, 24)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn iso_timestamps_become_epoch_seconds() {
        assert_eq!(iso_epoch("1970-01-01T00:00:00+00:00"), Some(0.0));
        assert_eq!(iso_epoch("2000-03-01T00:00:00Z"), Some(951868800.0));
        let t = iso_epoch("2026-10-24T20:40:00.500000+00:00").unwrap();
        assert!((t - 1792874400.5).abs() < 1.0, "{t}");
        assert_eq!(iso_epoch("garbage"), None);
    }

    #[test]
    fn replies_keep_only_what_the_panel_shows_and_the_view_names_the_biggest_cost() {
        let ctx = json!({"totalTokens": 20000, "maxTokens": 1000000, "model": "m", "categories": [
            {"name": "System prompt", "tokens": 3000, "kind": "used"}, {"name": "Skills", "tokens": 10000, "kind": "used"},
            {"name": "Free space", "tokens": 900000, "kind": "free"}, {"name": "Autocompact buffer", "tokens": 33000, "kind": "buffer"},
            {"name": "Later tools", "tokens": 15000, "kind": "deferred"}],
            "messageBreakdown": {"toolCallTokens": 1, "toolResultTokens": 2, "attachmentTokens": 5000},
            "skills": {"totalSkills": 78, "tokens": 10000}, "memoryFiles": [{"path": "x", "tokens": 1}]});
        let c = parse_context(&ctx).unwrap();
        assert_eq!((c.cats.len(), c.skills, c.attachments), (5, (78, 10000), 5000));
        assert!(parse_context(&json!({"nope": 1})).is_none());
        let plan = parse_plan(&json!({"rate_limits": {"five_hour": {"utilization": 41, "resets_at": "2026-10-24T20:40:00+00:00"}, "seven_day": {"utilization": 12.5, "resets_at": null}, "seven_day_opus": null}}));
        assert_eq!(plan.iter().map(|l| (l.name.as_str(), l.percent)).collect::<Vec<_>>(), [("5-hour", 41.0), ("weekly", 12.5)]);
        let u = Usage { ctx: Some(c), plan, asked: true };
        let v = view(&u, 100, 1792874400.0 - 3.0 * 3600.0 - 600.0);
        let text: Vec<String> = v.body.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert!(text[0].contains("5-hour") && text[0].contains("41%") && text[0].contains("resets in 3h 10m"), "{text:?}");
        let order: Vec<&String> = text.iter().filter(|t| t.starts_with("  ") && !t.contains("messages")).collect();
        assert!(order[0].contains("Skills") && order[0].contains("50%"), "biggest used first: {order:?}");
        assert!(order.last().unwrap().contains("Later tools") || order.last().unwrap().contains("Autocompact"), "deferred and reserved last: {order:?}");
        assert!(v.tail.iter().any(|l| l.spans.first().is_some_and(|s| s.content.contains("Skills take"))), "the hint names the cost");
        assert_eq!((tokens(950), tokens(9959), tokens(1_000_000)), ("950".into(), "10.0k".into(), "1.0M".into()));
        assert_eq!((until(30.0), until(3700.0), until(200000.0)), ("now".into(), "1h 1m".into(), "2d 7h".into()));
    }
}
