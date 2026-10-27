//! One conversation: its backend process, transcript, pending prompts, and usage.
//! Protocol messages are kept as `serde_json::Value` so unknown/new fields from
//! upstream Claude Code versions are ignored instead of breaking deserialization.

use crate::AppEvent;
use crate::backend::{Backend, Launch, Msg, Resume};
use crate::history;
use crate::image::{self, Shown};
use ratatui::text::Line;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::time::Instant;

/// One piece of a user message that contains pasted images.
pub enum Part {
    Text(String),
    /// Image number and, if the terminal can show it, how to draw it.
    Image(u32, Option<Shown>),
}

/// A pasted image waiting for the message it was pasted into.
pub struct Attachment {
    pub n: u32,
    pub name: String,
    /// The API content block (base64).
    pub block: Value,
    /// PNG the terminal can draw, kept so showing it again does not re-convert.
    pub png: Option<Vec<u8>>,
    /// The small preview shown under its `[Image #n]` token in the input box.
    pub preview: Option<Shown>,
}

pub enum Entry {
    User(String),
    /// A message with pasted images, in the order the user wrote it.
    Rich(Vec<Part>),
    Assistant { text: String, msg_id: String, streaming: bool },
    Tool { id: String, name: String, input: Value, result: Option<(String, bool)>, nested: bool },
    Note(String),
    Error(String),
    /// A picture already sent to the terminal; drawn as placeholder cells.
    Image(Shown),
}

/// A model Claude Code offers (from the `initialize` reply), for the picker.
#[derive(Clone)]
pub struct ModelInfo {
    /// What `set_model` takes: an alias like `sonnet`.
    pub value: String,
    pub name: String,
    pub desc: String,
    /// Full model id it currently resolves to, to tell which one is active.
    pub resolved: String,
    pub efforts: Vec<String>,
    /// Supports fast mode (Opus only).
    pub fast: bool,
}

/// A slash command Claude Code offers (from the `initialize` reply), for completion.
#[derive(Clone)]
pub struct Cmd {
    pub name: String,
    pub desc: String,
    pub hint: String,
}

pub struct Item {
    pub entry: Entry,
    /// Rendered lines for a given width; dropped whenever the entry changes.
    pub cache: Option<(u16, Vec<Line<'static>>)>,
}

pub struct Pending {
    pub request_id: String,
    pub tool: String,
    pub input: Value,
    pub suggestions: Value,
    /// Cursor, checkboxes, typed text and the like for whatever panel this prompt is shown in.
    pub ui: crate::prompt::Ui,
}

impl Pending {
    pub fn questions(&self) -> &[Value] {
        self.input["questions"].as_array().map(Vec::as_slice).unwrap_or(&[])
    }
    pub fn is_question(&self) -> bool {
        self.tool == "AskUserQuestion" && !self.questions().is_empty()
    }
}

#[derive(Clone, Copy)]
pub enum Choice {
    Allow,
    Always,
    Deny,
}

enum Start {
    New,
    Resume(String),
    Continue,
}

pub struct Session {
    pub id: u64,
    pub title: String,
    pub backend: Option<Backend>,
    /// Directory this tab works in; `None` follows the directory claude++ started in.
    pub cwd: Option<String>,
    /// Resume as a copy: the first spawn adds `--fork-session`.
    pub fork: bool,
    /// `/find`: the item last jumped to, and the item the next frame should bring into view.
    pub found: Option<usize>,
    pub find_query: String,
    pub reveal: Option<usize>,
    /// Last `/usage` data: the context breakdown and plan limits Claude Code reported.
    pub usage: crate::usage::Usage,
    pub lean_hinted: bool,
    /// The message box while another tab is in front: (text, cursor).
    pub draft: (String, usize),
    /// This tab's own tool rules, (rule, allow?), sent as `permissions` and re-sent when the process restarts.
    pub rules: Vec<(String, bool)>,
    /// Tool names the session reported.
    pub tools: Vec<String>,
    /// Transcript rows in the last frame and its height, so paging and anchoring know the viewport.
    pub view_h: usize,
    pub last_total: usize,
    pub session_id: Option<String>,
    start: Start,
    pub items: Vec<Item>,
    pub pending: VecDeque<Pending>,
    pub busy: bool,
    pub busy_since: Instant,
    pub activity: String,
    pub model: String,
    pub mode: String,
    /// Prompt size of the latest top-level API request (≈ context window in use).
    pub ctx_tokens: u64,
    pub cost: f64,
    pub cache_read: u64,
    pub prompt_total: u64,
    pub out_tokens: u64,
    pub rate: String,
    pub last_active: Instant,
    /// Launch profile for this tab (see `profile_args` in main.rs).
    pub profile: String,
    /// When the last API response arrived; the prompt cache's lifetime is measured from here.
    pub last_api: Option<Instant>,
    pub cold_noted: bool,
    /// Launch with `--system-prompt-snapshot off` so this tab's prompt follows its profile instead of
    /// the one pinned when the conversation started. Costs a full re-cache when first applied.
    pub unpinned: bool,
    /// Lines scrolled up from the bottom; 0 = follow output.
    pub scroll: usize,
    pub attachments: Vec<Attachment>,
    /// Slash commands this backend reported; `cmds_new` tells the app to adopt them once.
    pub commands: Vec<Cmd>,
    pub cmds_new: bool,
    pub models: Vec<ModelInfo>,
    pub models_new: bool,
    /// Effort level the backend reports (`get_settings`); empty until known.
    pub effort: String,
    /// What the user picked, re-applied whenever the backend is restarted (suspend/resume).
    pub model_pref: Option<String>,
    pub effort_pref: Option<String>,
    /// Fast mode as the backend last reported it, and why it is unavailable if it said so.
    pub fast_mode: bool,
    pub fast_reason: String,
    /// What the user chose, re-applied whenever the backend is restarted.
    pub fast_pref: Option<bool>,
    /// A fast-mode change we asked for and have not yet heard the outcome of (and how many older
    /// `initialize` replies to ignore first, when the change was queued before the backend started).
    fast_expect: Option<bool>,
    fast_skip: u8,
    /// While replaying a transcript: do not re-send old images to the terminal.
    replaying: bool,
    // ---- what a status line command is told about this session ----
    /// (percent used, reset time as unix seconds) for the 5-hour and 7-day usage windows.
    pub five_hour: Option<(f64, f64)>,
    pub seven_day: Option<(f64, f64)>,
    lines_added: u64,
    lines_removed: u64,
    api_ms: u64,
    started: Instant,
    pub ctx_window: u64,
    version: String,
    output_style: String,
    last_usage: Value,
    /// The command's latest output, parsed; empty until it has run once.
    pub status_lines: Vec<Line<'static>>,
    pub status_dirty: bool,
    pub status_running: bool,
    pub status_ran: Option<Instant>,
    /// Generation of the latest run; results of cancelled older runs are ignored.
    pub status_gen: u64,
    pub status_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Unix time of the next thing the last JSON said would change by itself (a limit window reset, the
    /// prompt cache expiring), when the line should refresh without any other event.
    pub status_next: Option<f64>,
    /// Cache lifetime assumed for `prompt_cache` (the app's `--cache-ttl`).
    pub cache_ttl: std::time::Duration,
    // prompt-cache statistics for the main conversation (`prompt_cache` in the status line JSON)
    req_id: String,
    requests: u64,
    misses: u64,
    expected_rebuilds: u64,
    cache_write: u64,
    miss_recache: u64,
    last_miss_at: Option<f64>,
    miss_causes: std::collections::BTreeMap<String, u64>,
    last_miss_cause: Option<String>,
    caching_observed: bool,
    /// Tokens the previous request had cached (read + written): what the next one could have read.
    prev_cached: u64,
    pub last_req_at: Option<Instant>,
    /// A /compact just happened, so the next shortfall in cache reads is expected, not a miss.
    rebuild_expected: bool,
    /// Control requests to send once a backend exists (mode/model changes while suspended).
    deferred: Vec<(String, Value)>,
    live: Vec<(u64, usize)>,
    cur_msg: String,
    turn_text: bool,
    interrupted: bool,
    pub mem_kb: u64,
}

impl Entry {
    /// The words of this entry, for search.
    fn text(&self) -> String {
        match self {
            Entry::User(t) | Entry::Note(t) | Entry::Error(t) => t.clone(),
            Entry::Rich(parts) => parts.iter().filter_map(|p| if let Part::Text(t) = p { Some(t.as_str()) } else { None }).collect::<Vec<_>>().join(" "),
            Entry::Assistant { text, .. } => text.clone(),
            Entry::Tool { name, input, result, .. } => format!("{name} {input} {}", result.as_ref().map_or("", |r| r.0.as_str())),
            Entry::Image(_) => String::new(),
        }
    }
}

impl Session {
    pub fn new(id: u64, title: String) -> Self {
        Session {
            id,
            title,
            backend: None,
            cwd: None,
            fork: false,
            found: None,
            find_query: String::new(),
            reveal: None,
            usage: Default::default(),
            lean_hinted: false,
            draft: Default::default(),
            rules: vec![],
            tools: vec![],
            view_h: 20,
            last_total: 0,
            session_id: None,
            start: Start::New,
            items: Vec::new(),
            pending: VecDeque::new(),
            busy: false,
            busy_since: Instant::now(),
            activity: String::new(),
            model: String::new(),
            mode: String::new(),
            ctx_tokens: 0,
            cost: 0.0,
            cache_read: 0,
            prompt_total: 0,
            out_tokens: 0,
            rate: String::new(),
            last_active: Instant::now(),
            profile: String::new(),
            last_api: None,
            cold_noted: false,
            unpinned: false,
            scroll: 0,
            attachments: Vec::new(),
            commands: Vec::new(),
            cmds_new: false,
            models: Vec::new(),
            models_new: false,
            effort: String::new(),
            model_pref: None,
            effort_pref: None,
            fast_mode: false,
            fast_reason: String::new(),
            fast_pref: None,
            fast_expect: None,
            fast_skip: 0,
            replaying: false,
            five_hour: None,
            seven_day: None,
            lines_added: 0,
            lines_removed: 0,
            api_ms: 0,
            started: Instant::now(),
            ctx_window: 0,
            version: String::new(),
            output_style: String::new(),
            last_usage: Value::Null,
            status_lines: Vec::new(),
            status_dirty: true,
            status_running: false,
            status_ran: None,
            status_gen: 0,
            status_cancel: Default::default(),
            status_next: None,
            cache_ttl: std::time::Duration::from_secs(3600),
            req_id: String::new(),
            requests: 0,
            misses: 0,
            expected_rebuilds: 0,
            cache_write: 0,
            miss_recache: 0,
            last_miss_at: None,
            miss_causes: Default::default(),
            last_miss_cause: None,
            caching_observed: false,
            prev_cached: 0,
            last_req_at: None,
            rebuild_expected: false,
            deferred: Vec::new(),
            live: Vec::new(),
            cur_msg: String::new(),
            turn_text: false,
            interrupted: false,
            mem_kb: 0,
        }
    }

    pub fn resume(&mut self, id: String) {
        match history::find(&id) {
            Some(path) => self.show_history(&path),
            None => self.note(format!("resuming session {id} (transcript not found, so earlier messages are not shown)")),
        }
        self.start = Start::Resume(id);
    }

    pub fn continue_last(&mut self, cwd: &str) {
        match history::recent(cwd, 1).first().and_then(|s| history::find(&s.id)) {
            Some(path) => self.show_history(&path),
            None => self.note("continuing the most recent conversation in this directory".into()),
        }
        self.start = Start::Continue;
    }

    fn show_history(&mut self, path: &std::path::Path) {
        let (lines, skipped) = history::tail(path);
        if skipped > 0 {
            self.note(format!("… {skipped} earlier messages not shown"));
        }
        for v in &lines {
            self.replay(v);
        }
        self.note("— resumed —".into());
        // The cache clock runs from the transcript's last write, not from the replay.
        self.last_api = std::fs::metadata(path).and_then(|m| m.modified()).ok()
            .and_then(|m| Instant::now().checked_sub(m.elapsed().unwrap_or_default()));
    }

    /// Feed one transcript line: real prompts become user entries; assistant text, tool calls and
    /// tool results go through the same path as live messages.
    fn replay(&mut self, v: &Value) {
        self.replaying = true;
        if v["type"] == "user" {
            match history::user_text(v) {
                Some(t) => self.push(Entry::User(t)),
                None if v["message"]["content"].as_array().is_some_and(|a| a.iter().any(|b| b["type"] == "tool_result")) => {
                    self.handle(v)
                }
                None => {}
            }
        } else {
            self.handle(v);
        }
        self.replaying = false;
    }

    /// Send an API image block to the terminal and add it to the transcript. Where images are off
    /// (or on replay) nothing is added: the text label already says an image was there.
    fn push_image(&mut self, block: &Value) {
        if self.replaying || !image::enabled() {
            return;
        }
        let src = &block["source"];
        let bytes = src["data"].as_str().and_then(image::unbase64).unwrap_or_default();
        self.push_picture(&bytes, src["media_type"].as_str().unwrap_or("image/png"));
    }

    fn push_picture(&mut self, bytes: &[u8], media: &str) {
        if self.replaying || !image::enabled() {
            return;
        }
        let cols = ratatui::crossterm::terminal::size().map_or(80, |s| s.0 as u32).saturating_sub(6);
        match image::show(bytes, media, cols) {
            Some(s) => self.push(Entry::Image(s)),
            None => self.note("(image could not be displayed)".into()),
        }
    }

    /// Prompt-cache bookkeeping for one top-level API request, using Claude Code's definitions: a *miss*
    /// re-processed content the previous request had cached (a shortfall in cache reads of at least 2,000
    /// tokens and 5% of it), unless a compaction explains it.
    fn account_request(&mut self, read: u64, created: u64) {
        let now = unix_now();
        self.requests += 1;
        self.cache_write += created;
        self.caching_observed |= read + created > 0;
        let shortfall = self.prev_cached.saturating_sub(read);
        if self.requests > 1 && shortfall >= 2000 && shortfall * 20 > self.prev_cached {
            if std::mem::take(&mut self.rebuild_expected) {
                self.expected_rebuilds += 1;
            } else {
                self.misses += 1;
                self.miss_recache += created;
                self.last_miss_at = Some(now);
                // The one cause we can name from outside: the cache outlived its time to live.
                self.last_miss_cause = self
                    .last_req_at
                    .filter(|t| t.elapsed() > self.cache_ttl)
                    .map(|_| format!("ttl_expired_{}", if self.cache_ttl.as_secs() >= 1800 { "1h" } else { "5m" }));
                if let Some(c) = &self.last_miss_cause {
                    *self.miss_causes.entry(c.clone()).or_default() += 1;
                }
            }
        }
        self.prev_cached = read + created;
        self.last_req_at = Some(Instant::now());
    }

    /// What Claude Code gives a status line command on stdin (same field names, per its docs), as far as
    /// headless mode tells us. Fields that are not known yet are null or absent exactly where Claude Code's
    /// are. `workspace.repo`, `git_worktree` and `session_name` are added later, off the UI thread.
    pub fn status_json(&mut self, cwd: &str) -> Value {
        let now = unix_now();
        // Several aliases can share a model id ("default" and "sonnet"); show the specific name.
        let name = self
            .models
            .iter()
            .filter(|m| !self.model.is_empty() && m.resolved == self.model)
            .min_by_key(|m| m.value == "default")
            .map_or(if self.model.is_empty() { "Claude" } else { self.model.as_str() }, |m| m.name.as_str());
        let window = if self.ctx_window > 0 { self.ctx_window } else { 200_000 };
        let started = self.ctx_tokens > 0;
        let round = |x: f64| (x * 100.0).round() / 100.0;
        let pct = started.then(|| round((self.ctx_tokens as f64 / window as f64 * 100.0).min(100.0)));
        let out_tokens = self.last_usage["output_tokens"].as_u64().unwrap_or(0);
        // Limit windows whose reset time has passed are dropped, as Claude Code does.
        let mut limits = serde_json::Map::new();
        let mut next: Option<f64> = None;
        for (k, w) in [("five_hour", self.five_hour), ("seven_day", self.seven_day)] {
            if let Some((p, r)) = w.filter(|(_, r)| *r > now) {
                limits.insert(k.into(), json!({"used_percentage": round(p), "resets_at": r as i64}));
                next = Some(next.map_or(r, |n: f64| n.min(r)));
            }
        }
        let mut v = json!({
            "session_id": self.session_id,
            "transcript_path": self.session_id.as_deref().and_then(history::find),
            "cwd": cwd,
            "model": {"id": self.model, "display_name": name},
            "workspace": {"current_dir": cwd, "project_dir": cwd, "added_dirs": []},
            "version": self.version,
            "output_style": {"name": self.output_style},
            "cost": {
                "total_cost_usd": self.cost,
                "total_duration_ms": self.started.elapsed().as_millis() as u64,
                "total_api_duration_ms": self.api_ms,
                "total_lines_added": self.lines_added,
                "total_lines_removed": self.lines_removed,
            },
            "context_window": {
                "total_input_tokens": self.ctx_tokens,
                "total_output_tokens": out_tokens,
                "context_window_size": window,
                "used_percentage": pct,
                "remaining_percentage": pct.map(|p| round(100.0 - p)),
                "current_usage": self.last_usage,
            },
            "exceeds_200k_tokens": self.ctx_tokens + out_tokens > 200_000,
            "fast_mode": self.fast_mode,
        });
        if !self.effort.is_empty() {
            v["effort"] = json!({"level": self.effort});
        }
        if !limits.is_empty() {
            v["rate_limits"] = Value::Object(limits);
        }
        if self.requests > 0 {
            let last = self.last_req_at.map_or(0.0, |t| t.elapsed().as_secs_f64());
            let ttl = self.cache_ttl.as_secs_f64();
            let warm = self.caching_observed && self.prev_cached > 0 && last < ttl;
            let expires = (self.prev_cached > 0).then(|| (now - last + ttl).floor());
            if let (true, Some(e)) = (warm, expires) {
                next = Some(next.map_or(e, |n| n.min(e)));
            }
            v["prompt_cache"] = json!({
                "warm": warm,
                "caching_observed": self.caching_observed,
                "ttl": if self.cache_ttl.as_secs() >= 1800 { "1h" } else { "5m" },
                "expires_at": expires.map(|e| e as i64),
                "requests": self.requests,
                "misses": self.misses,
                "expected_rebuilds": self.expected_rebuilds,
                "hit_ratio": (self.prompt_total > 0).then(|| (self.cache_read as f64 / self.prompt_total as f64 * 1000.0).round() / 1000.0),
                "cache_write_tokens": self.cache_write,
                "miss_recache_tokens": self.miss_recache,
                "last_miss_at": self.last_miss_at.map(|t| t as i64),
                "last_miss_cause": self.last_miss_cause.as_ref().map(|c| json!({"causes": [c]})),
                "miss_causes": self.miss_causes,
                "recache_tokens_if_cold": (!self.last_usage.is_null()).then_some(self.ctx_tokens),
            });
        }
        self.status_next = next;
        v
    }

    /// Switch model for this tab, and keep it across backend restarts.
    pub fn choose_model(&mut self, value: &str) {
        self.control("set_model", json!({"model": value}));
        self.model_pref = Some(value.into());
        self.control("get_settings", json!({}));
        // Fast mode follows the model: a model without it turns it off.
        let supports = self.models.iter().find(|m| m.value == value).is_none_or(|m| m.fast);
        if !supports && (self.fast_mode || self.fast_pref == Some(true)) {
            self.choose_fast(false);
        }
    }

    /// Turn fast mode on or off (Opus only, billed at a higher price per token). The outcome is reported
    /// back as a note once the backend says what state it ended up in.
    pub fn choose_fast(&mut self, on: bool) {
        self.fast_skip = u8::from(self.backend.is_none());
        self.fast_expect = Some(on);
        self.fast_pref = Some(on);
        self.control("apply_flag_settings", json!({"settings": {"fastMode": on}}));
        self.control("initialize", json!({}));
    }

    /// This tab's rule for a tool: `Some(true)` allow, `Some(false)` deny, `None` leave it to settings.
    pub fn rule(&self, name: &str) -> Option<bool> {
        self.rules.iter().find(|(r, _)| r == name).map(|r| r.1)
    }

    pub fn set_rule(&mut self, name: &str, state: Option<bool>) {
        self.rules.retain(|(r, _)| r != name);
        if let Some(allow) = state {
            self.rules.push((name.to_string(), allow));
        }
        if self.backend.is_some() {
            self.send_rules();
        }
    }

    /// The whole rule set, replacing whatever was sent before (an empty set clears it).
    fn send_rules(&mut self) {
        let pick = |allow: bool| self.rules.iter().filter(|r| r.1 == allow).map(|r| r.0.clone()).collect::<Vec<_>>();
        let permissions = json!({"allow": pick(true), "deny": pick(false)});
        self.control("apply_flag_settings", json!({"settings": {"permissions": permissions}}));
    }

    /// `/find`: jump to the newest entry containing `query` (case-insensitive) and remember the query.
    pub fn find(&mut self, query: &str) -> Option<(usize, usize)> {
        self.find_query = query.to_lowercase();
        self.found = None;
        self.find_step(true)
    }

    /// Step to the next older (or newer) match of the remembered query, wrapping. Returns (match number counted
    /// from the newest, total), or `None` when nothing matches.
    pub fn find_step(&mut self, older: bool) -> Option<(usize, usize)> {
        let hits: Vec<usize> = (0..self.items.len()).filter(|&i| self.items[i].entry.text().to_lowercase().contains(&self.find_query)).collect();
        let n = hits.len();
        let pos = match self.found.and_then(|f| hits.iter().position(|&h| h == f)) {
            Some(p) if older => (p + n - 1) % n,
            Some(p) => (p + 1) % n,
            None => n.checked_sub(1)?,
        };
        (self.found, self.reveal) = (Some(hits[pos]), Some(hits[pos]));
        Some((n - pos, n))
    }

    pub fn choose_effort(&mut self, level: &str) {
        self.control("apply_flag_settings", json!({"settings": {"effortLevel": level}}));
        self.effort_pref = Some(level.into());
        self.effort = level.into();
        self.control("get_settings", json!({}));
    }

    pub fn push(&mut self, entry: Entry) {
        self.items.push(Item { entry, cache: None });
    }

    /// A turn that ended (or a backend that died) mid-stream leaves no text block still "typing".
    fn end_streams(&mut self) {
        self.live.clear();
        for item in &mut self.items {
            if let Entry::Assistant { streaming, .. } = &mut item.entry
                && *streaming
            {
                *streaming = false;
                item.cache = None;
            }
        }
    }

    pub fn note(&mut self, s: String) {
        self.push(Entry::Note(s));
    }

    fn error(&mut self, s: String) {
        self.push(Entry::Error(s));
    }

    fn ensure_backend(&mut self, launch: &Launch, tx: &Sender<AppEvent>) -> std::io::Result<()> {
        if self.backend.is_some() {
            return Ok(());
        }
        // A conversation we already have an id for is resumed (e.g. after idle suspension).
        let resume = match (&self.session_id, &self.start) {
            (Some(id), _) => Resume::Id(id),
            (None, Start::Resume(id)) => Resume::Id(id),
            (None, Start::Continue) => Resume::Continue,
            (None, Start::New) => Resume::New,
        };
        let sid = self.id;
        let mut forked;
        let launch = if self.fork && self.session_id.is_none() && matches!(self.start, Start::Resume(_)) {
            forked = launch.clone();
            forked.args.push("--fork-session".into());
            &forked
        } else {
            launch
        };
        let mut b = Backend::spawn(launch, resume, tx.clone(), move |pid, m| AppEvent::Backend(sid, pid, m))?;
        if let Some(m) = &self.model_pref {
            b.control("set_model", json!({"model": m}))?;
        }
        if let Some(e) = &self.effort_pref {
            b.control("apply_flag_settings", json!({"settings": {"effortLevel": e}}))?;
        }
        if !self.rules.is_empty() {
            let pick = |allow: bool| self.rules.iter().filter(|r| r.1 == allow).map(|r| r.0.clone()).collect::<Vec<_>>();
            b.control("apply_flag_settings", json!({"settings": {"permissions": {"allow": pick(true), "deny": pick(false)}}}))?;
        }
        if self.fast_pref == Some(true) {
            b.control("apply_flag_settings", json!({"settings": {"fastMode": true}}))?;
            b.control("initialize", json!({}))?; // fast mode state is only reported by initialize
        }
        // Kept until every one is sent, so a backend that dies here leaves them for the next.
        for (sub, fields) in &self.deferred {
            b.control(sub, fields.clone())?;
        }
        self.deferred.clear();
        b.control("get_settings", json!({}))?;
        self.backend = Some(b);
        Ok(())
    }

    /// Spawn the backend ahead of the first message. Failures are left for `submit` to report.
    pub fn warm(&mut self, launch: &Launch, tx: &Sender<AppEvent>) {
        if self.backend.is_none() {
            let _ = self.ensure_backend(launch, tx);
        }
    }

    pub fn submit(&mut self, text: String, mentioned: &[std::path::PathBuf], launch: &Launch, tx: &Sender<AppEvent>) {
        let atts = std::mem::take(&mut self.attachments);
        let pieces = image::split_tokens(&text, |n| atts.iter().any(|a| a.n == n));
        let used = pieces.iter().any(|p| matches!(p, image::Piece::Image(_)));
        let plain = image::strip_tokens(&text);
        let content = if !used {
            self.push(Entry::User(text.clone()));
            json!(text)
        } else {
            // Text and pictures go to the model in the order they were written, each picture introduced by
            // its label so "compare Image #1 and #2" means something.
            let cols = ratatui::crossterm::terminal::size().map_or(80, |s| s.0 as u32).saturating_sub(6);
            let (mut blocks, mut parts): (Vec<Value>, Vec<Part>) = (vec![], vec![]);
            let n_pieces = pieces.len();
            let mut prev_image = false;
            for (i, piece) in pieces.into_iter().enumerate() {
                match piece {
                    image::Piece::Text(t) => {
                        // The paste put a newline before and after the token; they are not content.
                        let t = if prev_image { t.strip_prefix('\n').unwrap_or(&t) } else { &t };
                        let t = if i + 1 < n_pieces { t.strip_suffix('\n').unwrap_or(t) } else { t.trim_end() };
                        prev_image = false;
                        if !t.trim().is_empty() {
                            blocks.push(json!({"type": "text", "text": t}));
                            parts.push(Part::Text(t.to_string()));
                        }
                    }
                    image::Piece::Image(n) => {
                        prev_image = true;
                        let a = atts.iter().find(|a| a.n == n).unwrap();
                        blocks.push(json!({"type": "text", "text": image::token(n)}));
                        blocks.push(a.block.clone());
                        let shown = a.png.as_deref().filter(|_| image::enabled()).and_then(|p| image::upload(p, cols, 14));
                        parts.push(Part::Image(n, shown));
                    }
                }
            }
            self.push(Entry::Rich(parts));
            Value::Array(blocks)
        };
        for a in &atts {
            if let Some(p) = a.preview {
                image::delete(p.id); // the input-box preview is done
            }
        }
        if self.title.is_empty() || self.items.iter().all(|i| !matches!(i.entry, Entry::User(_) | Entry::Rich(_))) {
            self.title = plain.chars().take(18).collect::<String>().trim().to_string();
        }
        self.scroll = 0;
        self.last_active = Instant::now();
        let res = self
            .ensure_backend(launch, tx)
            .and_then(|_| self.backend.as_mut().unwrap().user(content));
        match res {
            Ok(()) => {
                if !self.busy {
                    self.busy = true;
                    self.busy_since = Instant::now();
                    self.turn_text = false;
                }
                self.activity = "waiting".into();
                // Claude Code reads `@image` mentions itself; this only shows them in the transcript.
                for path in mentioned {
                    if let (Some(media), Ok(bytes)) = (path.to_str().and_then(image::media_type), std::fs::read(path)) {
                        self.push_picture(&bytes, media);
                    }
                }
            }
            Err(e) => {
                self.backend = None;
                self.error(format!("could not reach claude: {e}"));
            }
        }
    }

    /// Send a control request now, or once the backend is started.
    pub fn control(&mut self, subtype: &str, fields: Value) {
        match self.backend.as_mut() {
            Some(b) => {
                if let Err(e) = b.control(subtype, fields) {
                    self.error(format!("control {subtype} failed: {e}"));
                }
            }
            None => self.deferred.push((subtype.into(), fields)),
        }
    }

    pub fn interrupt(&mut self) {
        if self.busy {
            self.control("interrupt", json!({}));
            self.pending.clear();
            self.activity = "interrupting".into();
            self.interrupted = true;
        }
    }

    /// Drop an idle backend to free memory; the next message resumes it from disk.
    pub fn can_suspend(&self) -> bool {
        self.backend.is_some()
            && !self.busy
            && self.pending.is_empty()
            && self.session_id.is_some()
            && !self.has_background_work()
    }

    pub fn suspend(&mut self) -> bool {
        if self.can_suspend() {
            self.backend = None;
            self.mem_kb = 0;
            return true;
        }
        false
    }

    /// Time left before the prompt cache for this conversation expires, if it has one.
    pub fn cache_left(&self, ttl: std::time::Duration) -> Option<std::time::Duration> {
        let t = self.last_api?;
        (self.ctx_tokens > 0).then(|| ttl.saturating_sub(t.elapsed()))
    }

    /// Sentence for a model switch. (Tool/profile changes did not invalidate the cache in our
    /// measurements, so they get no such warning.)
    pub fn recache_warning(&self) -> String {
        if self.ctx_tokens == 0 {
            return String::new();
        }
        format!(" Prompt caches are per model, so the next message likely re-caches ~{}k tokens.", self.ctx_tokens / 1000)
    }

    /// Stop the backend so the next message resumes with different launch flags. Refused mid-turn.
    pub fn restart_later(&mut self) -> Result<(), &'static str> {
        if self.busy || !self.pending.is_empty() {
            return Err("finish or interrupt the current turn first");
        }
        self.backend = None;
        self.mem_kb = 0;
        Ok(())
    }

    /// Heuristic: the session started something that outlives a turn (background shell, monitor,
    /// cron/wakeup), which a restart would silently lose.
    fn has_background_work(&self) -> bool {
        self.items.iter().any(|i| {
            matches!(&i.entry, Entry::Tool { name, input, .. }
                if name == "Monitor" || name == "ScheduleWakeup" || name.starts_with("Cron")
                    || input["run_in_background"] == true)
        })
    }

    /// Start a fresh conversation in this tab.
    pub fn clear(&mut self) {
        self.backend = None;
        self.session_id = None;
        self.start = Start::New;
        self.items.clear();
        self.pending.clear();
        self.busy = false;
        self.interrupted = false;
        self.ctx_tokens = 0;
        self.last_api = None;
        self.cold_noted = false;
        self.scroll = 0;
        self.mem_kb = 0;
        // A new conversation starts its counters over (Claude Code resets the cost on /clear too).
        self.cost = 0.0;
        self.lines_added = 0;
        self.lines_removed = 0;
        self.api_ms = 0;
        self.started = Instant::now();
        self.last_usage = Value::Null;
        self.requests = 0;
        self.misses = 0;
        self.expected_rebuilds = 0;
        self.cache_write = 0;
        self.miss_recache = 0;
        self.last_miss_at = None;
        self.miss_causes.clear();
        self.last_miss_cause = None;
        self.caching_observed = false;
        self.prev_cached = 0;
        self.req_id.clear();
        self.cache_read = 0;
        self.prompt_total = 0;
        self.status_lines.clear();
        self.status_dirty = true;
    }

    pub fn answer(&mut self, choice: Choice) {
        let Some(p) = self.pending.pop_front() else { return };
        let resp = match choice {
            Choice::Allow => json!({"behavior": "allow", "updatedInput": p.input}),
            Choice::Always => json!({
                "behavior": "allow",
                "updatedInput": p.input,
                "updatedPermissions": p.suggestions,
            }),
            Choice::Deny => json!({"behavior": "deny", "message": "The user declined this action."}),
        };
        self.respond(&p.request_id, resp);
    }

    /// Send the finished answers of the front AskUserQuestion back as the tool's input.
    pub fn submit_answers(&mut self, answers: Vec<(String, String)>) {
        let Some(p) = self.pending.pop_front() else { return };
        let mut input = p.input.clone();
        input["answers"] = Value::Object(answers.into_iter().map(|(q, a)| (q, json!(a))).collect());
        self.respond(&p.request_id, json!({"behavior": "allow", "updatedInput": input}));
    }

    fn respond(&mut self, request_id: &str, resp: Value) {
        if let Some(b) = self.backend.as_mut()
            && let Err(e) = b.respond(request_id, resp) {
                self.error(format!("could not answer permission request: {e}"));
            }
    }

    pub fn on_backend(&mut self, pid: u32, msg: Msg) {
        // Ignore stragglers from a backend we already dropped (suspend/clear).
        if self.backend.as_ref().map(|b| b.pid) != Some(pid) {
            return;
        }
        self.last_active = Instant::now();
        match msg {
            Msg::Line(v) => self.handle(&v),
            Msg::Exited { status, stderr_tail } => {
                self.backend = None;
                self.pending.clear();
                self.end_streams();
                // An interrupt the backend died before answering has nothing left to silence.
                self.interrupted = false;
                if self.busy || !stderr_tail.is_empty() {
                    self.busy = false;
                    let tail = if stderr_tail.is_empty() { String::new() } else { format!("\n{stderr_tail}") };
                    self.error(format!("claude {status}{tail}"));
                }
            }
        }
    }

    pub fn handle(&mut self, v: &Value) {
        let top_level = v["parent_tool_use_id"].is_null();
        let kind = v["type"].as_str().unwrap_or("");
        // Output after a `result` means a queued message started another turn.
        if !self.busy && !self.replaying && matches!(kind, "stream_event" | "assistant") {
            self.busy = true;
            self.busy_since = Instant::now();
            self.turn_text = false;
        }
        match kind {
            "system" => match v["subtype"].as_str().unwrap_or("") {
                "init" => {
                    let first = self.session_id.is_none();
                    self.session_id = v["session_id"].as_str().map(String::from);
                    self.model = v["model"].as_str().unwrap_or_default().into();
                    self.version = v["claude_code_version"].as_str().unwrap_or_default().into();
                    self.fast_mode = v["fast_mode_state"] == "on";
                    if let Some(t) = v["tools"].as_array() {
                        self.tools = t.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                    }
                    self.output_style = v["output_style"].as_str().unwrap_or("default").into();
                    self.status_dirty |= !self.busy;
                    if self.mode.is_empty() || first {
                        self.mode = v["permissionMode"].as_str().unwrap_or_default().into();
                    }
                }
                "compact_boundary" => {
                    self.note("conversation compacted".into());
                    // Docs: current_usage is null again until the next API call, and the cache rebuild that
                    // follows is expected rather than a miss.
                    self.last_usage = Value::Null;
                    self.rebuild_expected = true;
                    self.status_dirty = true;
                }
                "status" => {
                    if let Some(s) = v["status"].as_str() {
                        self.activity = s.into();
                    }
                }
                _ => {}
            },
            "stream_event" if top_level => self.stream(&v["event"]),
            "assistant" => {
                let msg = &v["message"];
                let id = msg["id"].as_str().unwrap_or_default().to_string();
                for block in msg["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or("") {
                        "text" if top_level => {
                            let text = block["text"].as_str().unwrap_or_default().to_string();
                            self.turn_text = true;
                            let live = self.items.iter_mut().find(|i| {
                                matches!(&i.entry, Entry::Assistant { msg_id, streaming: true, .. } if *msg_id == id)
                            });
                            match live {
                                Some(item) => {
                                    item.entry = Entry::Assistant { text, msg_id: id.clone(), streaming: false };
                                    item.cache = None;
                                }
                                None if !text.trim().is_empty() => {
                                    self.push(Entry::Assistant { text, msg_id: id.clone(), streaming: false })
                                }
                                None => {}
                            }
                        }
                        "tool_use" => self.push(Entry::Tool {
                            id: block["id"].as_str().unwrap_or_default().into(),
                            name: block["name"].as_str().unwrap_or("?").into(),
                            input: block["input"].clone(),
                            result: None,
                            nested: !top_level,
                        }),
                        _ => {}
                    }
                }
                if top_level {
                    let u = &msg["usage"];
                    let n = |k: &str| u[k].as_u64().unwrap_or(0);
                    let ctx = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
                    if ctx > 0 {
                        self.last_usage = json!({
                            "input_tokens": n("input_tokens"),
                            "output_tokens": n("output_tokens"),
                            "cache_creation_input_tokens": n("cache_creation_input_tokens"),
                            "cache_read_input_tokens": n("cache_read_input_tokens"),
                        });
                    }
                    if ctx > 0 {
                        self.ctx_tokens = ctx;
                        self.last_api = Some(Instant::now());
                        self.cold_noted = false;
                        if self.req_id != id {
                            self.req_id = id.clone();
                            self.account_request(n("cache_read_input_tokens"), n("cache_creation_input_tokens"));
                        }
                    }
                }
            }
            "user" => {
                let content = &v["message"]["content"];
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or("") {
                        "tool_result" => {
                            let id = block["tool_use_id"].as_str().unwrap_or_default();
                            let text = result_text(&block["content"]);
                            let err = block["is_error"].as_bool().unwrap_or(false);
                            if let Some(item) = self
                                .items
                                .iter_mut()
                                .rev()
                                .find(|i| matches!(&i.entry, Entry::Tool { id: t, .. } if t == id))
                            {
                                if let Entry::Tool { result, name, input, .. } = &mut item.entry {
                                    if !err && !self.replaying {
                                        let (a, r) = line_delta(name, input);
                                        self.lines_added += a;
                                        self.lines_removed += r;
                                    }
                                    *result = Some((text, err));
                                }
                                item.cache = None;
                            }
                            for b in block["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "image") {
                                self.push_image(b);
                            }
                        }
                        "text" if top_level => {
                            let t = block["text"].as_str().unwrap_or_default();
                            if !t.is_empty() {
                                self.note(t.into());
                            }
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                self.busy = false;
                self.pending.clear();
                self.end_streams();
                self.activity.clear();
                self.cost += v["total_cost_usd"].as_f64().unwrap_or(0.0);
                self.api_ms += v["duration_api_ms"].as_u64().unwrap_or(0);
                if let Some(state) = v["fast_mode_state"].as_str() {
                    self.fast_mode = state == "on";
                }
                // The model's real context window size, which only the result reports.
                let usage = v["modelUsage"].as_object();
                let window = usage.and_then(|m| m.get(&self.model).or_else(|| m.values().next())).and_then(|m| m["contextWindow"].as_u64());
                self.ctx_window = window.unwrap_or(self.ctx_window);
                self.status_dirty = true;
                let u = &v["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                self.cache_read += n("cache_read_input_tokens");
                self.prompt_total += n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
                self.out_tokens += n("output_tokens");
                let text = v["result"].as_str().unwrap_or_default();
                if std::mem::take(&mut self.interrupted) {
                    // The "[Request interrupted by user]" note already says it; the error result is noise.
                } else if v["is_error"].as_bool() == Some(true) || v["subtype"].as_str().is_some_and(|s| s.starts_with("error")) {
                    let errs = v["errors"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"))
                        .unwrap_or_default();
                    let msg = [text, &errs].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                    if !msg.is_empty() {
                        self.error(msg);
                    }
                } else if !self.turn_text && !text.trim().is_empty() {
                    // Local slash commands answer only through `result`.
                    self.note(text.into());
                }
            }
            "control_request" => {
                let req = &v["request"];
                let rid = v["request_id"].as_str().unwrap_or_default().to_string();
                if req["subtype"] == "can_use_tool" {
                    let input = req["input"].clone();
                    let questions = input["questions"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                    self.pending.push_back(Pending {
                        request_id: rid,
                        tool: req["tool_name"].as_str().unwrap_or("?").into(),
                        suggestions: req["permission_suggestions"].clone(),
                        ui: crate::prompt::Ui::for_questions(questions),
                        input,
                    });
                } else if let Some(b) = self.backend.as_mut() {
                    // Hooks/SDK-MCP callbacks are never registered by claude++; refuse rather than hang.
                    let _ = b.respond_error(&rid, "not supported by claude++");
                }
            }
            // The CLI withdrew a prompt (aborted, or settled by a hook): stop asking.
            "control_cancel_request" => self.pending.retain(|p| v["request_id"].as_str() != Some(p.request_id.as_str())),
            "control_response" => {
                let r = &v["response"];
                let err = r["error"].as_str().unwrap_or("?");
                // Older Claude Code versions lack some of the requests we probe with; that is not news.
                if r["subtype"] == "error" && !err.starts_with("Unsupported control request") {
                    self.note(format!("backend refused a request: {err}"));
                }
                if let Some(c) = crate::usage::parse_context(&r["response"]) {
                    self.usage.ctx = Some(c);
                    self.usage.asked = true;
                }
                if r["response"]["rate_limits"].is_object() {
                    self.usage.plan = crate::usage::parse_plan(&r["response"]);
                }
                if let Some(e) = r["response"]["applied"]["effort"].as_str() {
                    self.status_dirty |= !self.busy && self.effort != e;
                    self.effort = e.into();
                }
                // The init event only arrives with a turn; get_settings knows the model sooner, and right after
                // a switch.
                if let Some(m) = r["response"]["applied"]["model"].as_str() {
                    self.status_dirty |= !self.busy && self.model != m;
                    self.model = m.into();
                }
                if let Some(state) = r["response"]["fast_mode_state"].as_str() {
                    self.fast_mode = state == "on";
                    self.fast_reason = r["response"]["fast_mode_disabled_reason"].as_str().unwrap_or_default().into();
                    self.status_dirty |= !self.busy;
                    if self.fast_expect.is_some() && self.fast_skip > 0 {
                        self.fast_skip -= 1; // the reply to the initialize sent at spawn, before our change
                    } else if let Some(want) = self.fast_expect.take() {
                        let msg = match (want, self.fast_mode) {
                            (true, true) => "Fast mode ON ↯ — same model, faster output, higher price per token.".to_string(),
                            (false, false) => "Fast mode OFF".to_string(),
                            (true, false) => format!("Fast mode could not be turned on ({}).", if self.fast_reason.is_empty() { "the model or your account does not support it" } else { self.fast_reason.as_str() }),
                            (false, true) => "Fast mode is still on.".to_string(),
                        };
                        self.note(msg);
                    }
                }
                if let Some(list) = r["response"]["models"].as_array() {
                    let s = |c: &Value, k: &str| c[k].as_str().unwrap_or_default().to_string();
                    self.models = list
                        .iter()
                        .map(|m| ModelInfo {
                            value: s(m, "value"),
                            name: s(m, "displayName"),
                            desc: s(m, "description"),
                            resolved: s(m, "resolvedModel"),
                            efforts: m["supportedEffortLevels"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect(),
                            fast: m["supportsFastMode"] == true,
                        })
                        .filter(|m| !m.value.is_empty())
                        .collect();
                    self.models_new = true;
                }
                if let Some(list) = r["response"]["commands"].as_array() {
                    let s = |c: &Value, k: &str| c[k].as_str().unwrap_or_default().to_string();
                    self.commands = list
                        .iter()
                        .map(|c| Cmd { name: s(c, "name"), desc: s(c, "description"), hint: s(c, "argumentHint") })
                        .filter(|c| !c.name.is_empty())
                        .collect();
                    self.cmds_new = true;
                }
            }
            "rate_limit_event" => {
                let i = &v["rate_limit_info"];
                for (key, slot) in [("five_hour", &mut self.five_hour), ("seven_day", &mut self.seven_day)] {
                    let w = &i["unifiedWindows"][key];
                    if let (Some(u), Some(r)) = (w["utilization"].as_f64(), w["resetsAt"].as_f64()) {
                        *slot = Some((u * 100.0, r));
                    }
                }
                // Mid-turn events wait for the end-of-turn refresh instead of triggering their own.
                self.status_dirty |= !self.busy;
                self.rate = match (i["status"].as_str(), i["utilization"].as_f64()) {
                    (Some("allowed") | None, _) => String::new(),
                    (Some(_), Some(u)) => {
                        format!("limit {} {:.0}%", i["rateLimitType"].as_str().unwrap_or(""), u * 100.0)
                    }
                    (Some(s), None) => format!("limit {s}"),
                };
            }
            "_raw" => self.note(v["text"].as_str().unwrap_or_default().into()),
            _ => {}
        }
    }

    fn stream(&mut self, e: &Value) {
        match e["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.cur_msg = e["message"]["id"].as_str().unwrap_or_default().into();
                self.live.clear();
                self.activity = "responding".into();
            }
            "content_block_start" => {
                let idx = e["index"].as_u64().unwrap_or(0);
                let cb = &e["content_block"];
                match cb["type"].as_str().unwrap_or("") {
                    "text" => {
                        self.push(Entry::Assistant { text: String::new(), msg_id: self.cur_msg.clone(), streaming: true });
                        self.live.push((idx, self.items.len() - 1));
                        self.activity = "responding".into();
                    }
                    "thinking" | "redacted_thinking" => self.activity = "thinking".into(),
                    "tool_use" => self.activity = format!("calling {}", cb["name"].as_str().unwrap_or("tool")),
                    _ => {}
                }
            }
            "content_block_delta" => {
                let d = &e["delta"];
                if d["type"] != "text_delta" {
                    return;
                }
                let idx = e["index"].as_u64().unwrap_or(0);
                if let Some(&(_, i)) = self.live.iter().find(|(b, _)| *b == idx)
                    && let Some(item) = self.items.get_mut(i)
                        && let Entry::Assistant { text, .. } = &mut item.entry {
                            text.push_str(d["text"].as_str().unwrap_or_default());
                            item.cache = None;
                        }
            }
            _ => {}
        }
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// (added, removed) lines for an edit tool call: lines that differ once the unchanged lines at the start
/// and end of old/new are set aside. A Write counts all its lines as added (the old file is not known).
fn line_delta(tool: &str, input: &Value) -> (u64, u64) {
    fn diff(old: &str, new: &str) -> (u64, u64) {
        let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
        let head = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
        let tail = o[head..].iter().rev().zip(n[head..].iter().rev()).take_while(|(a, b)| a == b).count();
        ((n.len() - head - tail) as u64, (o.len() - head - tail) as u64)
    }
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    match tool {
        "Edit" => diff(&s(&input["old_string"]), &s(&input["new_string"])),
        "MultiEdit" => input["edits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| diff(&s(&e["old_string"]), &s(&e["new_string"])))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
        "Write" => (s(&input["content"]).lines().count() as u64, 0),
        _ => (0, 0),
    }
}

fn result_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
                Some("image") => "[image]".into(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &mut Session, lines: &[&str]) {
        for l in lines {
            s.handle(&serde_json::from_str(l).unwrap());
        }
    }

    #[test]
    fn withdrawn_prompts_cut_streams_and_a_cleared_interrupt_leave_nothing_stale() {
        let mut s = Session::new(1, String::new());
        s.busy = true;
        feed(
            &mut s,
            &[
                r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{},"permission_suggestions":[]}}"#,
                r#"{"type":"control_request","request_id":"r2","request":{"subtype":"can_use_tool","tool_name":"Read","input":{},"permission_suggestions":[]}}"#,
                r#"{"type":"control_cancel_request","request_id":"r1"}"#,
            ],
        );
        assert_eq!(s.pending.iter().map(|p| p.request_id.as_str()).collect::<Vec<_>>(), ["r2"], "the withdrawn prompt is gone, the next one is up");
        feed(
            &mut s,
            &[
                r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"id":"m1"}}}"#,
                r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
                r#"{"type":"result","subtype":"error_during_execution","is_error":true}"#,
            ],
        );
        assert!(matches!(&s.items[0].entry, Entry::Assistant { streaming: false, .. }), "no text left typing after the turn ends");
        assert!(!s.busy);
        feed(&mut s, &[r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"id":"m2"}}}"#]);
        assert!(s.busy, "a queued message's turn shows as busy again");
        s.interrupt();
        s.clear();
        feed(&mut s, &[r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"credit balance too low"}"#]);
        assert!(s.items.iter().any(|i| matches!(&i.entry, Entry::Error(t) if t.contains("credit"))), "a new conversation's error is not taken for the old interrupt");
    }

    #[test]
    fn streams_then_finalizes_text_and_tools() {
        let mut s = Session::new(1, String::new());
        s.busy = true;
        feed(
            &mut s,
            &[
                r#"{"type":"system","subtype":"init","session_id":"abc","model":"m","permissionMode":"manual"}"#,
                r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"message_start","message":{"id":"msg1"}}}"#,
                r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
                r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}}"#,
                r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}}"#,
            ],
        );
        assert!(matches!(&s.items[0].entry, Entry::Assistant { text, streaming: true, .. } if text == "Hello"));
        feed(
            &mut s,
            &[
                r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg1","content":[{"type":"text","text":"Hello!"}],"usage":{"input_tokens":2,"cache_read_input_tokens":100,"cache_creation_input_tokens":10}}}"#,
                r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
                r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[]}}"#,
                r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a\nb","is_error":false}]}}"#,
            ],
        );
        assert_eq!(s.items.len(), 2, "final text replaces the streamed entry");
        assert!(matches!(&s.items[0].entry, Entry::Assistant { text, streaming: false, .. } if text == "Hello!"));
        assert!(matches!(&s.items[1].entry, Entry::Tool { result: Some((r, false)), .. } if r == "a\nb"));
        assert_eq!(s.ctx_tokens, 112);
        assert_eq!(s.pending.len(), 1);
        assert_eq!(s.session_id.as_deref(), Some("abc"));
        feed(&mut s, &[r#"{"type":"result","subtype":"success","is_error":false,"result":"Hello!","total_cost_usd":0.5,"usage":{"input_tokens":1,"output_tokens":3}}"#]);
        assert!(!s.busy);
        assert!(s.pending.is_empty());
        assert_eq!(s.cost, 0.5);
        assert_eq!(s.items.len(), 2, "result text already shown is not duplicated");
    }

    #[test]
    fn result_only_turn_and_errors_are_shown() {
        let mut s = Session::new(1, String::new());
        feed(&mut s, &[r#"{"type":"result","subtype":"success","result":"Total cost: $0"}"#]);
        assert!(matches!(&s.items[0].entry, Entry::Note(t) if t == "Total cost: $0"));
        feed(&mut s, &[r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["boom"]}"#]);
        assert!(matches!(&s.items[1].entry, Entry::Error(t) if t == "boom"));
    }

    #[test]
    fn replay_rebuilds_prompts_text_tools_and_results() {
        let mut s = Session::new(1, String::new());
        let lines = [
            r#"{"type":"user","message":{"content":"fix the bug"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"<system-reminder>x"}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Looking."}]}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"a.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"fn main(){}"}]}}"#,
            r#"{"type":"user","message":{"content":"<local-command-stdout>noise</local-command-stdout>"}}"#,
        ];
        for l in lines {
            s.replay(&serde_json::from_str(l).unwrap());
        }
        assert_eq!(s.items.len(), 3, "meta and noise are dropped");
        assert!(matches!(&s.items[0].entry, Entry::User(t) if t == "fix the bug"));
        assert!(matches!(&s.items[1].entry, Entry::Assistant { text, streaming: false, .. } if text == "Looking."));
        assert!(matches!(&s.items[2].entry, Entry::Tool { result: Some((r, false)), .. } if r == "fn main(){}"));
        assert!(!s.busy && s.pending.is_empty());
    }

    #[test]
    fn background_work_blocks_suspension() {
        let mut s = Session::new(1, String::new());
        assert!(!s.has_background_work());
        s.push(Entry::Tool { id: "a".into(), name: "Bash".into(), input: json!({"command": "ls"}), result: None, nested: false });
        assert!(!s.has_background_work());
        s.push(Entry::Tool { id: "b".into(), name: "Bash".into(), input: json!({"command": "sleep 99", "run_in_background": true}), result: None, nested: false });
        assert!(s.has_background_work());
    }

    #[test]
    fn interrupt_error_result_is_suppressed() {
        let mut s = Session::new(1, String::new());
        s.busy = true;
        s.interrupt();
        feed(&mut s, &[r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["[ede_diagnostic] x"]}"#]);
        assert!(s.items.is_empty());
        assert!(!s.busy);
    }

    #[test]
    fn tab_rules_are_sent_whole_each_time_and_again_after_a_restart() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let launch = crate::backend::Launch { bin: script.to_string_lossy().into(), ..Default::default() };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut s = Session::new(1, String::new());
        s.set_rule("Bash", Some(false)); // no process yet: only remembered
        s.warm(&launch, &tx);
        s.set_rule("Read", Some(true));
        s.set_rule("Bash", None);
        assert_eq!((s.rule("Read"), s.rule("Bash")), (Some(true), None));
        drop(s);
        let sent = (0..60)
            .find_map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                std::fs::read_to_string(dir.join("stdin")).ok().filter(|t| t.matches("permissions").count() >= 2)
            })
            .expect("rules reached the process");
        let all: Vec<Value> = sent.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v["request"]["settings"]["permissions"].is_object()).collect();
        let perms = |v: &Value| (v["request"]["settings"]["permissions"]["allow"].clone(), v["request"]["settings"]["permissions"]["deny"].clone());
        assert_eq!(perms(&all[0]), (json!([]), json!(["Bash"])), "on start: what was chosen before the process existed");
        assert_eq!(perms(all.last().unwrap()), (json!(["Read"]), json!([])), "later sends replace the set; the removed rule is gone");
    }

    #[test]
    fn question_answers_are_sent_back_as_the_tools_input() {
        let dir = std::env::temp_dir().join(format!("claudeplusplus-qa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
        let launch = crate::backend::Launch { bin: script.to_string_lossy().into(), ..Default::default() };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut s = Session::new(1, String::new());
        s.warm(&launch, &tx);
        feed(
            &mut s,
            &[r#"{"type":"control_request","request_id":"q","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[{"question":"Color?","options":[{"label":"Red"},{"label":"Blue"}]},{"question":"Why?","options":[{"label":"A"}]}]}}}"#],
        );
        assert!(s.pending[0].is_question());
        s.submit_answers(vec![("Color?".into(), "Blue".into()), ("Why?".into(), "because".into())]);
        assert!(s.pending.is_empty(), "the request is consumed");
        drop(s);
        let sent = (0..60)
            .find_map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                std::fs::read_to_string(dir.join("stdin")).ok().filter(|t| t.contains("control_response"))
            })
            .expect("the answer reached the process");
        let resp: Value = sent.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find(|v| v["type"] == "control_response").unwrap();
        let r = &resp["response"]["response"];
        assert_eq!((resp["response"]["request_id"].as_str(), r["behavior"].as_str()), (Some("q"), Some("allow")));
        assert_eq!((r["updatedInput"]["answers"]["Color?"].as_str(), r["updatedInput"]["answers"]["Why?"].as_str()), (Some("Blue"), Some("because")));
        assert_eq!(r["updatedInput"]["questions"].as_array().map(Vec::len), Some(2), "the original questions are echoed");
    }

    #[test]
    fn models_effort_and_commands_are_read_from_control_responses() {
        let mut s = Session::new(1, String::new());
        feed(
            &mut s,
            &[
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"cpp-1","response":{"models":[{"value":"sonnet","displayName":"Sonnet 5.5","description":"d","resolvedModel":"claude-sonnet-5-5","supportedEffortLevels":["low","high"]},{"value":""}],"commands":[{"name":"compact","description":"c","argumentHint":"[x]"}]}}}"#,
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"cpp-2","response":{"applied":{"model":"claude-sonnet-5-5","effort":"high"}}}}"#,
                r#"{"type":"control_response","response":{"subtype":"error","request_id":"cpp-3","error":"Unsupported control request subtype: get_settings"}}"#,
            ],
        );
        assert_eq!((s.models.len(), s.models[0].value.as_str(), s.models[0].efforts.len()), (1, "sonnet", 2));
        assert_eq!((s.effort.as_str(), s.commands.len(), s.models_new, s.cmds_new), ("high", 1, true, true));
        assert!(s.items.is_empty(), "an unsupported probe is not worth a note");
        s.choose_effort("low");
        assert_eq!((s.effort.as_str(), s.effort_pref.as_deref()), ("low", Some("low")));
    }

    #[test]
    fn status_json_follows_the_documented_schema_including_its_defaults() {
        let mut s = Session::new(1, String::new());
        let j = s.status_json("/p");
        assert!(j["rate_limits"].is_null(), "absent until the first API response");
        assert!(j["prompt_cache"].is_null(), "absent until the first API response");
        let c = &j["context_window"];
        assert_eq!((c["total_input_tokens"].as_u64(), c["total_output_tokens"].as_u64(), c["context_window_size"].as_u64()), (Some(0), Some(0), Some(200_000)), "0 before the first reply; 200000 unless known");
        assert!(c["used_percentage"].is_null() && c["current_usage"].is_null());
        assert_eq!((j["fast_mode"].as_bool(), j["exceeds_200k_tokens"].as_bool()), (Some(false), Some(false)));
        assert!(j["effort"].is_null(), "absent when the effort is not known");

        s.models = vec![
            ModelInfo { value: "default".into(), name: "Default (recommended)".into(), desc: String::new(), resolved: "claude-haiku-5-5".into(), efforts: vec![], fast: false },
            ModelInfo { value: "haiku".into(), name: "Haiku 5.5".into(), desc: String::new(), resolved: "claude-haiku-5-5".into(), efforts: vec![], fast: false },
        ];
        let future = unix_now() + 3600.0;
        let past = unix_now() - 60.0;
        feed(
            &mut s,
            &[
                r#"{"type":"system","subtype":"init","session_id":"abc","model":"claude-haiku-5-5","permissionMode":"default","claude_code_version":"2.1.310","output_style":"default","fast_mode_state":"on"}"#,
                &format!(r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"allowed","unifiedWindows":{{"five_hour":{{"utilization":0.14,"resetsAt":{future}}},"seven_day":{{"utilization":0.06,"resetsAt":{past}}}}}}}}}"#),
                r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"a","old_string":"keep\nold1\nold2\nkeep2","new_string":"keep\nnew1\nkeep2"}}],"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":100,"output_tokens":7}}}"#,
                r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}"#,
                r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m2","content":[{"type":"tool_use","id":"t2","name":"Write","input":{"file_path":"b","content":"a\nb\nc\n"}}]}}"#,
                r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"ok","is_error":false}]}}"#,
                r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m3","content":[{"type":"tool_use","id":"t3","name":"Edit","input":{"old_string":"x","new_string":"y\nz"}}]}}"#,
                r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"nope","is_error":true}]}}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"","total_cost_usd":0.5,"duration_api_ms":1200,"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":100},"modelUsage":{"claude-haiku-5-5":{"contextWindow":1000000}}}"#,
            ],
        );
        let j = s.status_json("/proj");
        assert_eq!((j["session_id"].as_str(), j["version"].as_str(), j["fast_mode"].as_bool()), (Some("abc"), Some("2.1.310"), Some(true)));
        assert!(j["hook_event_name"].is_null(), "not part of the documented status line schema");
        assert_eq!((j["model"]["id"].as_str(), j["model"]["display_name"].as_str()), (Some("claude-haiku-5-5"), Some("Haiku 5.5")), "the specific alias, not 'Default'");
        assert_eq!((j["workspace"]["current_dir"].as_str(), j["workspace"]["project_dir"].as_str(), j["cwd"].as_str()), (Some("/proj"), Some("/proj"), Some("/proj")));
        assert_eq!(j["rate_limits"]["five_hour"]["used_percentage"].as_f64().map(|p| p.round()), Some(14.0));
        assert!(j["rate_limits"]["seven_day"].is_null(), "a window past its reset time is dropped");
        assert_eq!(s.status_next.map(|t| (t - future).abs() < 1.0), Some(true), "the next self-driven refresh is when that window resets... unless the cache expires first");
        assert_eq!(j["cost"]["total_cost_usd"].as_f64(), Some(0.5));
        assert_eq!(j["cost"]["total_api_duration_ms"].as_u64(), Some(1200));
        // Edit: 2 old lines became 1 (unchanged edges ignored); Write: 3 lines; the failed edit counts for nothing
        assert_eq!((j["cost"]["total_lines_added"].as_u64(), j["cost"]["total_lines_removed"].as_u64()), (Some(4), Some(2)));
        let c = &j["context_window"];
        assert_eq!((c["total_input_tokens"].as_u64(), c["total_output_tokens"].as_u64(), c["context_window_size"].as_u64()), (Some(200), Some(7), Some(1_000_000)), "output is the last response's, not a running total");
        assert!((c["used_percentage"].as_f64().unwrap() - 0.02).abs() < 1e-9);
        assert_eq!(c["current_usage"]["cache_read_input_tokens"].as_u64(), Some(90));
        assert_eq!(j["exceeds_200k_tokens"], false);
        s.choose_effort("high");
        assert_eq!(s.status_json("/proj")["effort"]["level"], "high");
    }

    fn assistant(id: &str, read: u64, created: u64) -> String {
        format!(r#"{{"type":"assistant","parent_tool_use_id":null,"message":{{"id":"{id}","content":[{{"type":"text","text":"x"}}],"usage":{{"input_tokens":5,"cache_read_input_tokens":{read},"cache_creation_input_tokens":{created},"output_tokens":3}}}}}}"#)
    }

    #[test]
    fn prompt_cache_stats_count_requests_misses_and_expected_rebuilds_by_claude_codes_rules() {
        let mut s = Session::new(1, String::new());
        let feed_one = |s: &mut Session, id: &str, read, created| feed(s, &[&assistant(id, read, created)]);
        feed_one(&mut s, "a", 0, 10_000); // first request writes the cache
        let pc = s.status_json("/p")["prompt_cache"].clone();
        assert_eq!((pc["requests"].as_u64(), pc["misses"].as_u64(), pc["warm"].as_bool(), pc["caching_observed"].as_bool()), (Some(1), Some(0), Some(true), Some(true)));
        assert_eq!((pc["ttl"].as_str(), pc["cache_write_tokens"].as_u64()), (Some("1h"), Some(10_000)));
        assert!(pc["expires_at"].as_f64().unwrap() > unix_now() + 3000.0);
        assert!(pc["last_miss_cause"].is_null() && pc["last_miss_at"].is_null());

        feed_one(&mut s, "a", 0, 10_000); // the same message id again (streamed blocks) is not a new request
        feed_one(&mut s, "b", 10_000, 500); // healthy: read everything, wrote a little
        feed_one(&mut s, "c", 10_400, 900);
        assert_eq!((s.requests, s.misses), (3, 0), "{} {}", s.requests, s.misses);

        feed_one(&mut s, "d", 4_000, 7_300); // read far less than was cached: a miss
        assert_eq!((s.requests, s.misses, s.miss_recache), (4, 1, 7_300));
        assert!(s.last_miss_at.is_some() && s.last_miss_cause.is_none(), "cause unknown without more evidence");

        feed_one(&mut s, "e", 11_300, 100);
        s.rebuild_expected = true; // a /compact just happened
        feed_one(&mut s, "f", 1_000, 3_000);
        assert_eq!((s.misses, s.expected_rebuilds), (1, 1), "a rebuild after compaction is expected, not a miss");

        // an idle gap longer than the ttl is the one cause we can name
        s.last_req_at = Some(Instant::now() - std::time::Duration::from_secs(4000));
        feed_one(&mut s, "g", 0, 4_000);
        assert_eq!((s.misses, s.last_miss_cause.as_deref()), (2, Some("ttl_expired_1h")));
        let pc = s.status_json("/p")["prompt_cache"].clone();
        assert_eq!(pc["last_miss_cause"]["causes"][0], "ttl_expired_1h");
        assert_eq!(pc["miss_causes"]["ttl_expired_1h"], 1);

        // a short ttl is reported as 5m, and a cache past its ttl is not warm
        s.cache_ttl = std::time::Duration::from_secs(300);
        s.last_req_at = Some(Instant::now() - std::time::Duration::from_secs(400));
        let pc = s.status_json("/p")["prompt_cache"].clone();
        assert_eq!((pc["ttl"].as_str(), pc["warm"].as_bool()), (Some("5m"), Some(false)));
        // /clear starts the counters over
        s.clear();
        assert!(s.status_json("/p")["prompt_cache"].is_null());
    }

    #[test]
    fn compaction_and_clear_follow_the_docs() {
        let mut s = Session::new(1, String::new());
        feed(&mut s, &[&assistant("a", 0, 5000), r#"{"type":"result","subtype":"success","is_error":false,"result":"","total_cost_usd":0.4,"usage":{}}"#]);
        assert!(!s.status_json("/p")["context_window"]["current_usage"].is_null());
        s.status_dirty = false;
        feed(&mut s, &[r#"{"type":"system","subtype":"compact_boundary"}"#]);
        let j = s.status_json("/p");
        assert!(j["context_window"]["current_usage"].is_null(), "null again after /compact until the next API call");
        assert!(j["prompt_cache"]["recache_tokens_if_cold"].is_null());
        assert!(s.status_dirty, "/compact finishing refreshes the line");
        s.clear();
        assert_eq!(s.status_json("/p")["cost"]["total_cost_usd"].as_f64(), Some(0.0), "cost resets on /clear");
    }

    #[test]
    fn only_the_end_of_a_turn_marks_the_status_line_stale_while_busy() {
        let mut s = Session::new(1, String::new());
        assert_eq!(s.status_json("/p")["model"]["display_name"], "Claude", "before the model is known");
        s.status_dirty = false;
        s.busy = true;
        feed(
            &mut s,
            &[
                r#"{"type":"system","subtype":"init","session_id":"abc","model":"m"}"#,
                r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.1,"resetsAt":1}}}}"#,
                r#"{"type":"control_response","response":{"subtype":"success","request_id":"cpp-9","response":{"applied":{"effort":"low"}}}}"#,
            ],
        );
        assert!(!s.status_dirty, "mid-turn events do not each trigger a run");
        feed(&mut s, &[r#"{"type":"result","subtype":"success","is_error":false,"result":"","usage":{}}"#]);
        assert!(s.status_dirty);
        s.status_dirty = false;
        feed(&mut s, &[r#"{"type":"control_response","response":{"subtype":"success","request_id":"cpp-9","response":{"applied":{"effort":"high"}}}}"#]);
        assert!(s.status_dirty, "idle changes (an effort pick) do");
    }

    #[test]
    fn fast_mode_reports_back_once_and_ignores_the_spawn_time_reply() {
        let mut s = Session::new(1, String::new());
        let reply = |state: &str, reason: &str| format!(r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"x","response":{{"fast_mode_state":"{state}","fast_mode_disabled_reason":"{reason}"}}}}}}"#);
        // queued before the backend exists: the first initialize reply is the one sent at spawn
        s.choose_fast(true);
        assert!(s.fast_pref == Some(true) && s.fast_skip == 1);
        feed(&mut s, &[&reply("off", "sdk_opt_in_required")]);
        assert!(s.items.is_empty() && s.fast_expect.is_some(), "that reply predates our change");
        feed(&mut s, &[&reply("on", "")]);
        assert!(s.fast_mode);
        assert!(matches!(&s.items[0].entry, Entry::Note(t) if t.contains("Fast mode ON")));
        feed(&mut s, &[&reply("on", "")]);
        assert_eq!(s.items.len(), 1, "only one note per change");

        // refused: the reason is shown
        s.fast_skip = 0;
        s.fast_mode = false;
        s.fast_expect = Some(true);
        feed(&mut s, &[&reply("off", "org_disabled")]);
        assert!(matches!(&s.items[1].entry, Entry::Note(t) if t.contains("could not be turned on") && t.contains("org_disabled")));
        // a turn's result carries the state too
        feed(&mut s, &[r#"{"type":"result","subtype":"success","is_error":false,"result":"","fast_mode_state":"on","usage":{}}"#]);
        assert!(s.fast_mode);
        assert_eq!(s.status_json("/p")["fast_mode"], true);
    }

    #[test]
    fn models_report_fast_support() {
        let mut s = Session::new(1, String::new());
        feed(&mut s, &[r#"{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"models":[{"value":"opus","displayName":"Opus 5.5","resolvedModel":"claude-opus-5-5","supportsFastMode":true},{"value":"sonnet","displayName":"Sonnet","resolvedModel":"claude-sonnet-5-5"}]}}}"#]);
        assert_eq!(s.models.iter().map(|m| m.fast).collect::<Vec<_>>(), [true, false]);
    }

    #[test]
    fn subagent_messages_are_nested_and_not_streamed() {
        let mut s = Session::new(1, String::new());
        feed(
            &mut s,
            &[
                r#"{"type":"stream_event","parent_tool_use_id":"p","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#,
                r#"{"type":"assistant","parent_tool_use_id":"p","message":{"id":"m","content":[{"type":"text","text":"inner"},{"type":"tool_use","id":"t","name":"Read","input":{}}]}}"#,
            ],
        );
        assert_eq!(s.items.len(), 1);
        assert!(matches!(&s.items[0].entry, Entry::Tool { nested: true, .. }));
    }
}
