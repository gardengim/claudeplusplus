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
use std::time::{Duration, Instant};

/// One piece of a user message that contains pasted images.
pub enum Part {
    Text(String),
    /// Image number and, if the terminal can show it, how to draw it.
    Image(u32, Option<Shown>),
}

/// A message waiting for the running turn to end, with the pictures it mentions. With `due` it is a scheduled
/// one: it goes when that time has come and the tab is free.
pub struct Queued {
    pub text: String,
    pub atts: Vec<Attachment>,
    pub due: Option<std::time::SystemTime>,
}

impl Session {
    /// Stopping Claude, or losing it, pauses the messages that were waiting for the turn to end; scheduled
    /// ones wait for their time and are not part of that.
    fn hold_waiting(&mut self) {
        self.queue_hold |= self.queue.iter().any(|q| q.due.is_none());
    }
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

/// Result text of an agent call that is still working after its turn ended (Claude Code backgrounds agents).
pub const BACKGROUND: &str = "running in the background…";

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
    /// For a given width: how many rows the entry takes, and its rendered lines. Dropped whenever the
    /// entry changes; far from the view the lines alone are dropped (empty) and the count stays.
    pub cache: Option<(u16, usize, Vec<Line<'static>>)>,
    /// This tool call is shown the other way round from ctrl+o's global setting (a click or alt+o).
    pub flipped: bool,
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
    /// The conversation's `/color`: tints the tab and the UI's accent while it is in front.
    pub color: Option<String>,
    pub backend: Option<Backend>,
    /// Directory this tab works in; `None` follows the directory claude++ started in.
    pub cwd: Option<String>,
    /// The folder before the last `/cd`, for `/cd -`.
    pub prev_cwd: Option<String>,
    /// Resume as a copy: the first spawn adds `--fork-session`.
    pub fork: bool,
    /// The agent's task list (TaskCreate / TaskUpdate): (id, subject, status), shown above the input.
    pub tasks: Vec<(String, String, String)>,
    /// Shell commands Claude Code runs as tasks (`task_started` with `local_bash`): what ctrl+b
    /// backgrounds and `/tasks` lists and stops.
    pub shells: Vec<Shell>,
    /// This tab's terminals (`/term`, and the ones Claude opens). They outlive the `claude` process behind the tab and go with it.
    pub terms: std::sync::Arc<crate::term::Terms>,
    thinking_n: u64,
    /// `/find`: the item last jumped to, and the item the next frame should bring into view.
    pub found: Option<usize>,
    pub find_query: String,
    /// Messages sent while Claude works, in the order they go; held (not sent) after an interrupt or a crash.
    pub queue: Vec<Queued>,
    pub queue_hold: bool,
    pub reveal: Option<usize>,
    /// Last `/usage` data: the context breakdown and plan limits Claude Code reported.
    pub usage: crate::usage::Usage,
    pub lean_hinted: bool,
    pub long_hinted: bool,
    /// The note about shared MCP servers has been shown for this tab.
    pub share_hinted: bool,
    /// Whether claude may start in this folder (see `trust`), asked once and remembered; `/trust` clears it.
    pub trust: Option<Result<(), String>>,
    /// Off in tests, which run in folders of their own choosing.
    pub trust_gate: bool,
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
    /// Characters the model has streamed this turn (text, thinking, tool input): the spinner's token count.
    pub turn_chars: usize,
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
    /// Index into `items` where the current backend's life began: replayed history before it started nothing.
    bg_from: usize,
    /// A message was written to the current backend. Until then it has no conversation to resume.
    sent: bool,
    /// Permission mode chosen this session; a respawned backend starts in the launch default otherwise.
    mode_pref: Option<String>,
    /// Launch profile for this tab (see `profile_args` in main.rs).
    pub profile: String,
    /// When the last API response arrived; the prompt cache's lifetime is measured from here.
    pub last_api: Option<Instant>,
    pub cold_noted: bool,
    /// `/compact` was sent for this cache window (see `App::precompact`).
    pub precompacted: bool,
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
    /// Backends in a row that died before saying hello. At 2, typing no longer starts one (`warm`); only an
    /// explicit send tries again, so a broken start costs one error per message, not one per keystroke.
    failed_starts: u8,
    inited: bool,
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
    /// Messages a compaction kept; Claude Code sends them again after the boundary, already shown.
    preserved: Vec<String>,
    /// Output of `!command`s run since the last message, oldest first; it goes to the model ahead of the next
    /// one. The total is capped.
    shell_context: Vec<String>,
    /// Numbers `!command` entries; never reset, so a command finishing after /clear matches nothing.
    shell_n: u64,
    /// Steps already listed under each running Agent call, so the next one goes right after them.
    children: std::collections::HashMap<String, usize>,
    /// After a rewind: the transcript entry the next backend resumes at (`--resume-session-at`).
    resume_at: Option<String>,
    /// Files are being restored by a rewind: hold new messages until they are back.
    pub rewinding: bool,
    /// Control requests to send once a backend exists (mode/model changes while suspended).
    pub(crate) deferred: Vec<(String, Value)>,
    /// Since when a fresh backend's MCP servers may still be connecting: messages wait in `held` meanwhile, so
    /// the first request is not sent without their tools. None once they settled (or never in tests).
    pub(crate) mcp_wait: Option<Instant>,
    held: Vec<Value>,
    mcp_polled: Instant,
    live: Vec<(u64, usize)>,
    cur_msg: String,
    turn_text: bool,
    interrupted: bool,
    pub mem_kb: u64,
}

impl Entry {
    /// The words of this entry, for search.
    pub fn text(&self) -> String {
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
            color: None,
            backend: None,
            cwd: None,
            prev_cwd: None,
            fork: false,
            tasks: vec![],
            shells: vec![],
            terms: crate::term::Terms::new(std::sync::Arc::new(|| {})),
            thinking_n: 0,
            found: None,
            find_query: String::new(),
            queue: Vec::new(),
            queue_hold: false,
            reveal: None,
            usage: Default::default(),
            lean_hinted: false,
            long_hinted: false,
            share_hinted: false,
            trust: None,
            trust_gate: !cfg!(test),
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
            turn_chars: 0,
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
            bg_from: 0,
            sent: false,
            mode_pref: None,
            profile: String::new(),
            last_api: None,
            cold_noted: false,
            precompacted: false,
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
            failed_starts: 0,
            inited: false,
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
            preserved: Vec::new(),
            shell_context: Vec::new(),
            shell_n: 0,
            children: Default::default(),
            resume_at: None,
            rewinding: false,
            deferred: Vec::new(),
            mcp_wait: None,
            held: Vec::new(),
            mcp_polled: Instant::now(),
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
        // Pin the id now: `claude --continue` picks at backend launch, by which time another session in
        // this folder may be newer than the one shown.
        match history::recent(cwd, 1).into_iter().next() {
            Some(s) => self.resume(s.id),
            None => {
                self.note("continuing the most recent conversation in this directory".into());
                self.start = Start::Continue;
            }
        }
    }

    /// `/color <name>`: a colour of the palette sets it, `default` clears it, anything else is left for Claude Code to reject.
    pub fn set_color(&mut self, arg: &str) {
        match arg.trim().to_lowercase().as_str() {
            "default" => self.color = None,
            c if crate::theme::is_color(c) => self.color = Some(c.into()),
            _ => {}
        }
    }

    /// The conversation's name (`/rename`, else the generated one) is the tab's title.
    fn follow_name(&mut self, path: &std::path::Path) {
        if let Some(n) = history::session_name(path) {
            self.title = n.chars().take(18).collect::<String>().trim().to_string();
        }
    }

    fn show_history(&mut self, path: &std::path::Path) {
        self.follow_name(path);
        self.color = history::session_color(path);
        let (lines, skipped) = history::tail(path);
        if skipped > 0 {
            self.note(format!("… {skipped} earlier messages not shown"));
        }
        for v in &lines {
            self.replay(v);
        }
        // A foreground agent's report reached Claude through the agent's own transcript; the main one keeps
        // only a receipt. Show the report.
        let waiting: Vec<String> = self
            .items
            .iter()
            .filter_map(|i| match &i.entry {
                Entry::Tool { id, name, result: Some((r, false)), .. } if matches!(name.as_str(), "Agent" | "Task") && r.trim_start().starts_with(AGENT_RECEIPT) => Some(id.clone()),
                _ => None,
            })
            .collect();
        if !waiting.is_empty() {
            let reports = history::agent_reports(path);
            for id in waiting {
                let report = reports.get(&id).cloned().unwrap_or_else(|| "finished (its report was not saved)".into());
                self.set_result(&id, report, false);
            }
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
        let text = v["message"]["content"].as_str().or_else(|| v["message"]["content"][0]["text"].as_str()).unwrap_or_default();
        if v["type"] == "user" && text.starts_with("<task-notification>") {
            // In a saved transcript an agent's end is this message, not the live event: settle its Agent call.
            let tag = |name: &str| text.split(&format!("<{name}>")).nth(1).and_then(|r| r.split(&format!("</{name}>")).next()).map(str::trim);
            if let (Some(id), Some(summary)) = (tag("tool-use-id"), tag("summary")) {
                let running = self.items.iter().any(|i| {
                    matches!(&i.entry, Entry::Tool { id: t, result, .. } if t == id && result.as_ref().is_none_or(|r| r.0 == BACKGROUND))
                });
                if running {
                    self.set_result(id, summary.into(), tag("status") != Some("completed"));
                }
            }
        } else if v["type"] == "user" {
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
        let out_tokens = crate::backend::count(&self.last_usage["output_tokens"]);
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
            Some(p) => crate::panel::cycle(p, n, older),
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
        self.items.push(Item { entry, cache: None, flipped: false });
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
        self.push(Entry::Note(crate::theme::keys(&s)));
    }

    fn error(&mut self, s: String) {
        self.push(Entry::Error(s));
    }

    fn ensure_backend(&mut self, launch: &Launch, tx: &Sender<AppEvent>) -> std::io::Result<()> {
        if self.backend.is_some() {
            return Ok(());
        }
        if self.trust_gate {
            let cwd = launch.cwd.as_deref().map(std::path::PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
            if let Err(why) = self.trust.get_or_insert_with(|| crate::trust::check(&cwd)) {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, why.clone()));
            }
        }
        // A conversation we already have an id for is resumed (e.g. after idle suspension).
        let resume = match (&self.session_id, &self.start) {
            (Some(id), _) => Resume::Id(id),
            (None, Start::Resume(id)) => Resume::Id(id),
            (None, Start::Continue) => Resume::Continue,
            (None, Start::New) => Resume::New,
        };
        let sid = self.id;
        let mut launch = launch.clone();
        if self.fork && self.session_id.is_none() && matches!(self.start, Start::Resume(_)) {
            launch.args.push("--fork-session".into());
        }
        if let Some(at) = self.resume_at.take() {
            launch.args.extend(["--resume-session-at".into(), at]);
        }
        let launch = &launch;
        let mut b = Backend::spawn(launch, resume, tx.clone(), move |pid, m| AppEvent::Backend(sid, pid, m))?;
        if !launch.shared.is_empty() && !std::mem::replace(&mut self.share_hinted, true) {
            let names: Vec<&str> = launch.shared.iter().map(|(n, _)| n.as_str()).collect();
            self.note(format!("MCP servers shared with other tabs: {} ({} shared processes running). Plugin and claude.ai connector servers are not loaded in this mode; /config turns it off.", names.join(", "), crate::share::live()));
        }
        if let Some(m) = &self.model_pref {
            b.control("set_model", json!({"model": m}))?;
        }
        if let Some(e) = &self.effort_pref {
            b.control("apply_flag_settings", json!({"settings": {"effortLevel": e}}))?;
        }
        if let Some(m) = &self.mode_pref {
            b.control("set_permission_mode", json!({"mode": m}))?;
        }
        (self.bg_from, self.sent) = (self.items.len(), false);
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
        self.inited = false;
        self.mcp_wait = (!cfg!(test)).then(Instant::now);
        Ok(())
    }

    /// Spawn the backend ahead of the first message. Failures are left for `submit` to report.
    pub fn warm(&mut self, launch: &Launch, tx: &Sender<AppEvent>) {
        if self.backend.is_none() && self.failed_starts < 2 {
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
        // A /command must stay first to be a command; the shell output waits for a real message.
        let ctx = if text.starts_with('/') { String::new() } else { std::mem::take(&mut self.shell_context).concat() };
        let content = match ctx {
            ctx if ctx.is_empty() => content,
            ctx => match content {
                Value::String(t) => json!(format!("{ctx}{t}")),
                Value::Array(mut b) => {
                    b.insert(0, json!({"type": "text", "text": ctx}));
                    Value::Array(b)
                }
                v => v,
            },
        };
        let res = self
            .ensure_backend(launch, tx)
            .and_then(|_| match self.mcp_wait {
                Some(_) => {
                    self.held.push(content);
                    self.mcp_polled = Instant::now();
                    self.backend.as_mut().unwrap().control("mcp_status", json!({}))
                }
                None => self.backend.as_mut().unwrap().user(content),
            });
        match res {
            Ok(()) => {
                self.sent = true;
                if !self.busy {
                    self.busy = true;
                    self.busy_since = Instant::now();
                    self.turn_text = false;
                    self.turn_chars = 0;
                }
                self.activity = if self.held.is_empty() { "waiting" } else { "connecting MCP servers" }.into();
                // Claude Code reads `@image` mentions itself; this only shows them in the transcript.
                for path in mentioned {
                    if let (Some(media), Ok(bytes)) = (path.to_str().and_then(image::media_type), std::fs::read(path)) {
                        self.push_picture(&bytes, media);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => self.error(e.to_string()),
            Err(e) => {
                self.backend = None;
                self.error(format!("could not reach claude: {e}"));
            }
        }
    }

    /// The permission mode the backend moved to by itself (an answered prompt): shown, and kept for a respawn.
    fn set_mode(&mut self, mode: &str) {
        self.mode = mode.into();
        self.mode_pref = Some(mode.into());
    }

    /// Send a control request now, or once the backend is started.
    pub fn control(&mut self, subtype: &str, fields: Value) {
        if subtype == "set_permission_mode" {
            self.mode_pref = fields["mode"].as_str().map(String::from);
        }
        match self.backend.as_mut() {
            Some(b) => {
                if let Err(e) = b.control(subtype, fields) {
                    self.error(format!("control {subtype} failed: {e}"));
                }
            }
            None => self.deferred.push((subtype.into(), fields)),
        }
    }

    /// Called every tick: while a message waits for MCP servers, ask how they are doing (4× a second), and send
    /// it anyway after 5 s, so a slow or older server never blocks the conversation.
    pub fn poll_mcp(&mut self) {
        let Some(since) = self.mcp_wait else { return };
        if self.held.is_empty() {
            return;
        }
        if since.elapsed() > Duration::from_secs(5) {
            self.release_held();
        } else if self.mcp_polled.elapsed() >= Duration::from_millis(250) {
            self.mcp_polled = Instant::now();
            self.control("mcp_status", json!({}));
        }
    }

    fn release_held(&mut self) {
        self.mcp_wait = None;
        let held = std::mem::take(&mut self.held);
        if !held.is_empty() && self.activity == "connecting MCP servers" {
            self.activity = "waiting".into();
        }
        for content in held {
            if let Err(e) = self.backend.as_mut().map_or(Err(std::io::Error::other("no backend")), |b| b.user(content)) {
                self.error(format!("could not reach claude: {e}"));
                return;
            }
        }
    }

    pub fn interrupt(&mut self) {
        if !self.held.is_empty() {
            // Nothing reached Claude yet: drop what was waiting rather than stopping a turn that never began.
            self.held.clear();
            self.busy = false;
            self.note("not sent".into());
            return;
        }
        if self.busy {
            // Stopping Claude is not a request to start on the next message.
            self.hold_waiting();
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
            // The init event (and the id) only arrive with a turn, so a backend nobody has sent to has nothing to resume.
            && (self.session_id.is_some() || !self.sent)
            && !self.has_background_work()
            // Stopping the process would kill them.
            && !self.shells.iter().any(|s| s.status.is_none())
    }

    /// ctrl+b: move the command running in the foreground to the background, so the turn goes on.
    pub fn background_shell(&mut self) -> &'static str {
        let running = |s: &&mut Shell| s.status.is_none() && !s.background;
        match self.shells.iter_mut().rev().find(running) {
            Some(s) => {
                s.background = true;
                let id = s.tool_use_id.clone();
                self.control("background_tasks", json!({"tool_use_id": id}));
                "moved to the background: /tasks lists it"
            }
            // Claude Code registers a command as a task a few seconds after it starts.
            None if self.busy && self.items.iter().rev().any(|i| matches!(&i.entry, Entry::Tool { name, result: None, .. } if name == "Bash")) => {
                "the command has only just started: press ctrl+b again in a moment"
            }
            None => "no command running in the foreground",
        }
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
    /// The Claude Code version this tab's process reported ("" before its first start).
    pub fn claude_version(&self) -> &str {
        &self.version
    }

    /// Idle, running, at least `min` tokens, and the cache has under `PRECOMPACT_LEAD` left: the moment to compact.
    pub fn should_precompact(&self, ttl: std::time::Duration, min: u64) -> bool {
        const PRECOMPACT_LEAD: std::time::Duration = std::time::Duration::from_secs(30);
        !self.precompacted
            && self.backend.is_some()
            && !self.busy
            && !self.rewinding
            && self.pending.is_empty()
            && self.queue.is_empty()
            && self.ctx_tokens >= min
            && self.cache_left(ttl).is_some_and(|l| !l.is_zero() && l <= PRECOMPACT_LEAD)
    }

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

    /// Earlier messages this tab can rewind to: (item index, text).
    pub fn rewind_points(&self) -> Vec<(usize, String)> {
        let points = self.items.iter().enumerate().filter_map(|(i, it)| Some((i, user_said(&it.entry)?)));
        points.filter(|(_, t)| !t.starts_with('/')).collect()
    }

    /// Whether Claude edited files at or after item `at`, so restoring code would change something.
    pub fn edits_since(&self, at: usize) -> bool {
        let edit = |n: &str| matches!(n, "Edit" | "MultiEdit" | "Write" | "NotebookEdit");
        self.items[at.min(self.items.len())..].iter().any(|i| {
            matches!(&i.entry, Entry::Tool { name, input, .. }
                if edit(name) || input["_steps"].as_array().is_some_and(|a| a.iter().any(|s| edit(s["name"].as_str().unwrap_or_default()))))
        })
    }

    /// Go back to just before the message at item `at`, as Claude Code's double-Esc does: the conversation
    /// (the next message resumes at the transcript entry before it), the files edited since (restored from
    /// Claude Code's checkpoints by `--rewind-files`, off the UI thread), or both. A conversation rewind
    /// returns the message's text, to edit and send again.
    pub fn rewind(&mut self, at: usize, conversation: bool, code: bool, launch: &Launch, tx: &Sender<AppEvent>) -> Result<Option<String>, String> {
        let Some(text) = self.items.get(at).and_then(|i| user_said(&i.entry)) else { return Err("no such message".into()) };
        let id = match (&self.session_id, &self.start) {
            (Some(id), _) | (None, Start::Resume(id)) => id.clone(),
            _ => return Err("nothing saved to rewind yet".into()),
        };
        let path = history::find(&id).ok_or("the saved conversation was not found")?;
        let want = prompt_words(&text);
        let later = self.items[at + 1..].iter().filter(|i| user_said(&i.entry).is_some_and(|t| prompt_words(&t) == want)).count();
        // Only prompts on the branch shown: an earlier rewind leaves abandoned ones in the file.
        let prompt = saved_prompt(history::prompts(&path), &text, later).ok_or("that message is not in the saved conversation")?;
        let old_pid = self.backend.as_ref().map(|b| b.pid);
        self.restart_later().map_err(String::from)?;
        if code {
            self.rewinding = true;
            let uuid = prompt["uuid"].as_str().unwrap_or_default().to_string();
            let mut cmd = std::process::Command::new(&launch.bin);
            cmd.args(["-p", "--resume", &id, "--rewind-files", &uuid])
                .envs(launch.env.iter().cloned())
                .stdin(std::process::Stdio::null());
            if let Some(dir) = &launch.cwd {
                cmd.current_dir(dir);
            }
            let (tx, sid) = (tx.clone(), self.id);
            std::thread::spawn(move || {
                // The dropped backend gets up to a second to exit; restore files only once it has.
                if let Some(pid) = old_pid {
                    let gone = || unsafe { libc::kill(pid as i32, 0) } != 0;
                    let deadline = Instant::now() + std::time::Duration::from_secs(2);
                    while !gone() && Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                }
                let said = match cmd.output() {
                    Ok(o) if String::from_utf8_lossy(&o.stdout).contains("Files rewound") => Ok("files restored to before that message".into()),
                    Ok(o) => Err(String::from_utf8_lossy(if o.stdout.is_empty() { &o.stderr } else { &o.stdout }).trim().replace("Error: ", "")),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(AppEvent::Rewound(sid, said));
            });
        }
        if !conversation {
            return Ok(None);
        }
        match prompt["parentUuid"].as_str() {
            Some(p) => self.resume_at = Some(p.to_string()),
            // Nothing before the first message: a new conversation (and nothing to resume at).
            None => (self.session_id, self.start, self.resume_at) = (None, Start::New, None),
        }
        self.items.truncate(at);
        self.live.retain(|(_, i)| *i < at);
        self.children.clear();
        self.preserved.clear();
        self.shell_context.clear();
        self.ctx_tokens = 0;
        self.last_usage = Value::Null;
        self.interrupted = false;
        self.scroll = 0;
        self.note("rewound: the conversation continues from here".into());
        // Pictures do not come back with the words: drop their tokens rather than leave dangling ones.
        Ok(Some(image::strip_tokens(&text).trim().to_string()))
    }

    /// Heuristic: the session started something that outlives a turn (background shell, monitor,
    /// cron/wakeup), which a restart would silently lose.
    fn has_background_work(&self) -> bool {
        self.items.iter().skip(self.bg_from).any(|i| outlives_a_turn(&i.entry))
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
        self.queue.clear();
        self.queue_hold = false;
        self.tasks.clear();
        self.shells.clear();
        self.shell_context.clear();
        self.children.clear();
        self.ctx_tokens = 0;
        self.last_api = None;
        self.cold_noted = false;
        self.precompacted = false;
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
        // Claude Code changes mode without telling us; keep the label honest.
        let set_mode = p.suggestions.as_array().into_iter().flatten().find(|s| s["type"] == "setMode").and_then(|s| s["mode"].as_str());
        match (choice, set_mode) {
            (Choice::Always, Some(m)) => self.set_mode(m),
            (Choice::Allow, _) if p.tool == "ExitPlanMode" => self.set_mode("default"),
            _ => {}
        }
        let resp = match choice {
            Choice::Allow => json!({"behavior": "allow", "updatedInput": p.input}),
            Choice::Always => json!({
                "behavior": "allow",
                "updatedInput": p.input,
                "updatedPermissions": p.suggestions,
            }),
            Choice::Deny if p.ui.feedback.trim().is_empty() => json!({"behavior": "deny", "message": "The user declined this action."}),
            Choice::Deny => json!({
                "behavior": "deny",
                "message": format!("The user declined this action and said what to do instead: {}", p.ui.feedback.trim()),
            }),
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
                if !self.inited {
                    self.failed_starts = self.failed_starts.saturating_add(1);
                }
                self.pending.clear();
                self.hold_waiting();
                self.end_streams();
                // An interrupt the backend died before answering has nothing left to silence.
                self.interrupted = false;
                // The conversation to resume no longer exists: forget it, or every keystroke (`warm`) would
                // spawn `--resume <id>` again and fail the same way.
                if stderr_tail.contains("No conversation found") {
                    self.session_id = None;
                    self.start = Start::New;
                    (self.fork, self.resume_at) = (false, None);
                    self.busy = false;
                    self.error(format!("claude {status}\n{stderr_tail}\nnot resuming it again; your next message starts a new conversation"));
                    return;
                }
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
        if let Some(i) = v["uuid"].as_str().and_then(|u| self.preserved.iter().position(|p| p == u)) {
            self.preserved.swap_remove(i);
            return;
        }
        // A new API call after a `result` means a queued message started another turn. (Only a real call
        // starts with `message_start`; a re-sent or late message must not leave the tab looking busy.)
        if !self.busy && !self.replaying && top_level && kind == "stream_event" && v["event"]["type"] == "message_start" {
            self.busy = true;
            self.busy_since = Instant::now();
            self.turn_text = false;
            self.turn_chars = 0;
        }
        match kind {
            "system" => match v["subtype"].as_str().unwrap_or("") {
                "init" => {
                    (self.inited, self.failed_starts) = (true, 0);
                    let first = self.session_id.is_none();
                    self.session_id = v["session_id"].as_str().map(String::from);
                    self.model = v["model"].as_str().unwrap_or_default().into();
                    self.version = v["claude_code_version"].as_str().unwrap_or_default().into();
                    if newer_than_verified(&self.version) && !WARNED_VERSION.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        self.note(format!("Claude Code {} is newer than the {VERIFIED_LINE}.x line claude++ was verified on; if something looks off, `/version` lists the installed versions and can pin an older one, and `python3 bench/contract.py` checks the protocol parts", self.version));
                    }
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
                // A background agent ended: its report (or why it failed) becomes the Agent call's result.
                "task_started" if v["task_type"] == "local_bash" => {
                    let id = v["task_id"].as_str().unwrap_or_default().to_string();
                    if !id.is_empty() && !self.shells.iter().any(|s| s.task_id == id) {
                        self.shells.push(Shell {
                            task_id: id,
                            tool_use_id: v["tool_use_id"].as_str().unwrap_or_default().into(),
                            desc: v["description"].as_str().unwrap_or_default().into(),
                            background: v["is_backgrounded"] == true,
                            status: None,
                            summary: String::new(),
                        });
                    }
                }
                // A shell's end, not an agent's report.
                "task_notification" if self.shells.iter().any(|s| v["task_id"].as_str() == Some(s.task_id.as_str())) => {
                    if let Some(sh) = self.shells.iter_mut().find(|s| v["task_id"].as_str() == Some(s.task_id.as_str())) {
                        sh.status = Some(v["status"].as_str().unwrap_or("done").into());
                        sh.summary = v["summary"].as_str().unwrap_or_default().into();
                    }
                }
                "task_notification" => {
                    let ok = v["status"] == "completed";
                    let id = v["tool_use_id"].as_str().unwrap_or_default();
                    let report = v["handback_report"]["text"].as_str();
                    // Without a report, the summary only replaces the "running" placeholder (or nothing).
                    let placeholder = self.items.iter().any(|i| {
                        matches!(&i.entry, Entry::Tool { id: t, result, .. } if t == id && result.as_ref().is_none_or(|r| r.0 == BACKGROUND))
                    });
                    if report.is_some() || !ok || placeholder {
                        let text = report.or(v["summary"].as_str()).unwrap_or_default();
                        self.set_result(id, text.trim().into(), !ok);
                    }
                }
                // A hook that failed or blocked something changes what happens next; say so, as Claude Code does.
                "hook_response" if v["outcome"].as_str().is_some_and(|o| o != "success") => {
                    let why = [&v["stderr"], &v["stdout"]].into_iter().filter_map(Value::as_str).find(|t| !t.trim().is_empty());
                    let why = why.and_then(|t| t.lines().find(|l| !l.trim().is_empty())).map(|l| format!(": {}", l.trim())).unwrap_or_default();
                    self.note(format!("hook {} {}{why}", v["hook_name"].as_str().unwrap_or("?"), v["outcome"].as_str().unwrap_or_default()));
                }
                "compact_boundary" => {
                    self.note("conversation compacted".into());
                    let meta = &v["compact_metadata"];
                    self.preserved =
                        meta["preserved_messages"]["uuids"].as_array().into_iter().flatten().filter_map(|u| u.as_str().map(String::from)).collect();
                    // Unknown until the next call: `post_tokens` counts only the messages, not the ~30k of
                    // system prompt and tools every request carries, so showing it would understate the context.
                    self.ctx_tokens = 0;
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
                        // Kept folded, like a finished tool call: ctrl+o or a click reads it.
                        "thinking" if top_level && block["thinking"].as_str().is_some_and(|t| !t.trim().is_empty()) => {
                            self.thinking_n += 1;
                            let id = format!("thinking-{}", self.thinking_n);
                            let text = block["thinking"].as_str().unwrap_or_default().to_string();
                            self.push(Entry::Tool { id, name: "Thinking".into(), input: json!({}), result: Some((text, false)), nested: false });
                        }
                        // The subagent's report: show it as the Agent call's result, not as a step.
                        "tool_use" if !top_level && block["name"] == "SubagentHandback" => {
                            let report = block["input"]["message"].as_str().unwrap_or_default().trim().to_string();
                            self.set_result(v["parent_tool_use_id"].as_str().unwrap_or_default(), report, false);
                        }
                        "tool_use" => {
                            let entry = Entry::Tool {
                                id: block["id"].as_str().unwrap_or_default().into(),
                                name: block["name"].as_str().unwrap_or("?").into(),
                                input: block["input"].clone(),
                                result: None,
                                nested: !top_level,
                            };
                            // A subagent's step goes under its own Agent call, so parallel agents do not interleave.
                            let parent = v["parent_tool_use_id"].as_str().unwrap_or_default();
                            let at = self.items.iter().position(|i| matches!(&i.entry, Entry::Tool { id, .. } if id == parent));
                            match at {
                                Some(at) if !top_level => {
                                    let n = self.children.entry(parent.to_string()).or_default();
                                    let pos = (at + 1 + *n).min(self.items.len());
                                    *n += 1;
                                    self.items.insert(pos, Item { entry, cache: None, flipped: false });
                                    // Text still streaming further down moved one place.
                                    self.live.iter_mut().filter(|(_, i)| *i >= pos).for_each(|(_, i)| *i += 1);
                                }
                                _ => self.push(entry),
                            }
                        }
                        _ => {}
                    }
                }
                if top_level {
                    let u = &msg["usage"];
                    let n = |k: &str| crate::backend::count(&u[k]);
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
                        self.precompacted = false;
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
                            let text = capped(result_text(&block["content"]));
                            let err = block["is_error"].as_bool().unwrap_or(false);
                            if let Some(item) = self
                                .items
                                .iter_mut()
                                .rev()
                                .find(|i| matches!(&i.entry, Entry::Tool { id: t, .. } if t == id))
                            {
                                if let Entry::Tool { result, name, input, .. } = &mut item.entry {
                                    if result.is_some() {
                                        continue; // a subagent's report already arrived; this is only its receipt
                                    }
                                    let text = if text.starts_with("Async agent launched") {
                                        BACKGROUND.to_string()
                                    } else {
                                        text
                                    };
                                    if !err && !self.replaying {
                                        let (a, r) = line_delta(name, input);
                                        self.lines_added += a;
                                        self.lines_removed += r;
                                    }
                                    // Edit/Write report the change as hunks with line numbers: show those, as Claude Code does.
                                    let extra = if v["tool_use_result"].is_object() { &v["tool_use_result"] } else { &v["toolUseResult"] };
                                    if !err && extra["structuredPatch"].as_array().is_some_and(|p| !p.is_empty()) && input.is_object() {
                                        input["_patch"] = extra["structuredPatch"].clone();
                                    }
                                    if !err {
                                        track_task(&mut self.tasks, name, input, &text);
                                    }
                                    *result = Some((text, err));
                                }
                                item.cache = None;
                            }
                            self.fold_steps(id);
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
                self.cost += v["total_cost_usd"].as_f64().unwrap_or(0.0).clamp(0.0, 1e9);
                self.api_ms += crate::backend::count(&v["duration_api_ms"]);
                if let Some(state) = v["fast_mode_state"].as_str() {
                    self.fast_mode = state == "on";
                }
                // The model's real context window size, which only the result reports.
                let usage = v["modelUsage"].as_object();
                let window = usage.and_then(|m| m.get(&self.model).or_else(|| m.values().next())).map(|m| crate::backend::count(&m["contextWindow"])).filter(|w| *w > 0);
                self.ctx_window = window.unwrap_or(self.ctx_window);
                self.status_dirty = true;
                if let Some(path) = self.session_id.as_deref().and_then(history::find) {
                    self.follow_name(&path);
                }
                let u = &v["usage"];
                let n = |k: &str| crate::backend::count(&u[k]);
                // Counts come from another process: saturate rather than overflow.
                self.cache_read = self.cache_read.saturating_add(n("cache_read_input_tokens"));
                let prompt = n("input_tokens").saturating_add(n("cache_read_input_tokens")).saturating_add(n("cache_creation_input_tokens"));
                self.prompt_total = self.prompt_total.saturating_add(prompt);
                self.out_tokens = self.out_tokens.saturating_add(n("output_tokens"));
                let text = v["result"].as_str().unwrap_or_default();
                if std::mem::take(&mut self.interrupted) {
                    // The "[Request interrupted by user]" note already says it; the error result is noise.
                } else if v["is_error"].as_bool() == Some(true) || v["subtype"].as_str().is_some_and(|s| s.starts_with("error")) {
                    let errs = v["errors"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"))
                        .unwrap_or_default();
                    let msg = [text, &errs].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                    // Claude Code often says it once as a reply too: show it once, as an error.
                    let said = !msg.trim().is_empty() && self.items.last().is_some_and(|i| matches!(&i.entry, Entry::Assistant { text, .. } if text.trim() == msg.trim()));
                    if said {
                        self.items.pop();
                    }
                    if !msg.is_empty() {
                        self.error(msg);
                    }
                } else if !self.turn_text && !text.trim().is_empty() {
                    // Local slash commands answer only through `result`.
                    self.note(text.into());
                } else if self.turn_chars > 0 {
                    // How long the model worked, as Claude Code says after each turn.
                    self.note(format!("✻ Worked for {}", duration(self.busy_since.elapsed())));
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
                        // Leaving plan mode: offer what Claude Code offers, approving and auto-accepting edits.
                        suggestions: match req["permission_suggestions"].as_array() {
                            Some(a) if !a.is_empty() => req["permission_suggestions"].clone(),
                            _ if req["tool_name"] == "ExitPlanMode" => {
                                json!([{"type": "setMode", "mode": "acceptEdits", "destination": "session"}])
                            }
                            _ => req["permission_suggestions"].clone(),
                        },
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
                if let Some(list) = r["response"]["mcpServers"].as_array()
                    && self.mcp_wait.is_some()
                    && !list.iter().any(|m| m["status"] == "pending")
                {
                    self.release_held();
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
                self.turn_chars += ["text", "thinking", "partial_json"].iter().find_map(|k| d[*k].as_str()).map_or(0, str::len);
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

/// How a foreground agent's tool result starts when its report went to Claude separately.
/// The Claude Code line `bench/contract.py` last passed on. A newer major.minor gets one note, not a block.
pub const VERIFIED_LINE: &str = "2.1";
static WARNED_VERSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn newer_than_verified(version: &str) -> bool {
    let mm = |v: &str| {
        let mut p = v.split('.').map(|x| x.parse::<u32>().ok());
        Some((p.next()??, p.next()??))
    };
    matches!((mm(version), mm(VERIFIED_LINE)), (Some(v), Some(k)) if v > k)
}

const AGENT_RECEIPT: &str = "This agent's report was delivered to you";

/// `7s`, `2m 5s`, `1h 3m`.
pub fn duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
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

const SHELL_CAP: usize = 30_000;

impl Session {
    /// A `!command` the user ran in this tab: show it like a Bash call, and keep its output for the model,
    /// in the form Claude Code itself uses, so the next message can refer to it.
    pub fn start_shell(&mut self, cmd: &str) -> String {
        self.shell_n += 1;
        let id = format!("shell-{}", self.shell_n);
        self.push(Entry::Tool { id: id.clone(), name: "Bash".into(), input: json!({"command": cmd}), result: None, nested: false });
        id
    }

    pub fn shell_done(&mut self, id: &str, cmd: &str, out: &str, err: &str, ok: bool) {
        // Claude Code keeps 30k characters of a command's output; so do we, stdout and stderr together.
        // stderr keeps up to a quarter of the budget: the error text of a failing command is what the model needs.
        let err_room = err.chars().count().min(SHELL_CAP / 4);
        let out: String = out.chars().take(SHELL_CAP - err_room).collect();
        let err: String = err.chars().take(SHELL_CAP - out.chars().count()).collect();
        let shown = [out.trim_end(), err.trim_end()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
        if !self.items.iter().any(|i| matches!(&i.entry, Entry::Tool { id: t, result: None, .. } if t == id)) {
            return; // the conversation it belonged to was cleared
        }
        self.set_result(id, shown, !ok);
        self.shell_context.push(format!("<bash-input>{cmd}</bash-input>\n<bash-stdout>{out}</bash-stdout>\n<bash-stderr>{err}</bash-stderr>\n\n"));
        // Several commands before one message must not add up past that either: the oldest go first.
        while self.shell_context.len() > 1 && self.shell_context.iter().map(|b| b.chars().count()).sum::<usize>() > SHELL_CAP {
            self.shell_context.remove(0);
        }
    }

    /// Fold or unfold one tool call; `None` is the latest one. Returns whether there was a call to flip.
    pub fn flip_call(&mut self, at: Option<usize>) -> bool {
        let i = at.or_else(|| self.items.iter().rposition(|i| matches!(i.entry, Entry::Tool { nested: false, .. })));
        let Some(item) = i.and_then(|i| self.items.get_mut(i)) else { return false };
        (item.flipped, item.cache) = (!item.flipped, None);
        true
    }

    fn set_result(&mut self, tool_id: &str, text: String, err: bool) {
        let text = capped(text);
        if let Some(item) = self.items.iter_mut().rev().find(|i| matches!(&i.entry, Entry::Tool { id, .. } if id == tool_id)) {
            if let Entry::Tool { result, .. } = &mut item.entry {
                *result = Some((text, err));
            }
            item.cache = None;
        }
        self.fold_steps(tool_id);
    }

    /// A finished agent's steps move into its Agent entry (`input._steps`), as Claude Code folds them:
    /// the report then reads right under the call, and ctrl+o still lists the steps.
    fn fold_steps(&mut self, parent: &str) {
        let Some(at) = self.items.iter().position(|i| {
            matches!(&i.entry, Entry::Tool { id, result: Some((r, _)), .. } if id == parent && r != BACKGROUND)
        }) else {
            return;
        };
        let n = self.children.remove(parent).unwrap_or(0).min(self.items.len() - at - 1);
        if n == 0 {
            return;
        }
        let steps: Vec<Value> = self
            .items
            .drain(at + 1..at + 1 + n)
            .filter_map(|i| match i.entry {
                Entry::Tool { name, input, .. } => Some(json!({"name": name, "input": input})),
                _ => None,
            })
            .collect();
        self.live.iter_mut().filter(|(_, i)| *i > at).for_each(|(_, i)| *i -= n);
        let item = &mut self.items[at];
        if let Entry::Tool { input, .. } = &mut item.entry
            && input.is_object()
        {
            let mut all = input["_steps"].as_array().cloned().unwrap_or_default();
            all.extend(steps);
            input["_steps"] = Value::Array(all);
        }
        item.cache = None;
    }
}

/// A tool call that keeps working after its turn ends, which a restart would lose.
fn outlives_a_turn(entry: &Entry) -> bool {
    let Entry::Tool { name, input, result, .. } = entry else { return false };
    let long_running_tool = name == "Monitor" || name == "ScheduleWakeup" || name.starts_with("Cron");
    // Claude Code backgrounds agents by default, without the flag; the agent's report replaces this text.
    let agent_still_working = matches!(name.as_str(), "Agent" | "Task") && result.as_ref().is_some_and(|r| r.0 == BACKGROUND);
    long_running_tool || input["run_in_background"] == true || agent_still_working
}

/// The words of a message you sent (pictures as their tokens), or None for any other entry.
fn user_said(e: &Entry) -> Option<String> {
    match e {
        Entry::User(t) => Some(t.clone()),
        Entry::Rich(parts) => Some(
            parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => t.clone(),
                    Part::Image(n, _) => image::token(*n),
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// A prompt's words, as shown and as saved alike (they differ in image tokens and spacing).
fn prompt_words(t: &str) -> String {
    image::strip_tokens(t).replace("[image]", "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The transcript entry of the prompt shown as `text`; the same words sent more than once are told
/// apart by `later`, how many of them come after it.
fn saved_prompt(prompts: Vec<Value>, text: &str, later: usize) -> Option<Value> {
    let want = prompt_words(text);
    prompts.into_iter().filter(|v| history::user_text(v).is_some_and(|t| prompt_words(&t) == want)).nth_back(later)
}

/// The most of one tool result kept for display.
const MAX_RESULT: usize = 512 * 1024;

/// `text`, or its first `MAX_RESULT` bytes and a note. Only for showing: Claude has the whole result, and a
/// 30 MB one would cost many times that here (the parsed line, copies, wrapped rows).
fn capped(mut text: String) -> String {
    if text.len() > MAX_RESULT {
        let cut = (0..=MAX_RESULT).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
        let more = text.len() - cut;
        text.truncate(cut);
        text.shrink_to_fit(); // truncate keeps the capacity
        text.push_str(&format!("\n… {} KB more not kept here", more / 1024));
    }
    text
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
mod tests;

/// Follow the task list: `TaskCreate` answers "Task #3 created successfully: <subject>", `TaskUpdate`
/// takes `{taskId, status?, subject?}` (status `deleted` removes it).
fn track_task(tasks: &mut Vec<(String, String, String)>, name: &str, input: &Value, result: &str) {
    match name {
        "TaskCreate" => {
            let Some(rest) = result.strip_prefix("Task #") else { return };
            let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
            let subject = input["subject"].as_str().map(String::from).or_else(|| rest.split_once(": ").map(|(_, s)| s.to_string()));
            if let (false, Some(subject)) = (id.is_empty(), subject) {
                tasks.retain(|t| t.0 != id);
                tasks.push((id, subject, "pending".into()));
            }
        }
        "TaskUpdate" => {
            let id = match &input["taskId"] {
                Value::String(s) => s.trim_start_matches('#').to_string(),
                v => v.to_string(),
            };
            if input["status"] == "deleted" {
                tasks.retain(|t| t.0 != id);
            } else if let Some(t) = tasks.iter_mut().find(|t| t.0 == id) {
                if let Some(st) = input["status"].as_str() {
                    t.2 = st.into();
                }
                if let Some(sub) = input["subject"].as_str() {
                    t.1 = sub.into();
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod task_tests {
    use serde_json::json;
    #[test]
    fn task_list_follows_create_update_and_delete() {
        let mut t = vec![];
        super::track_task(&mut t, "TaskCreate", &json!({"subject": "a"}), "Task #1 created successfully: a");
        super::track_task(&mut t, "TaskCreate", &json!({}), "Task #2 created successfully: b c");
        super::track_task(&mut t, "TaskUpdate", &json!({"taskId": "1", "status": "in_progress"}), "Updated task #1 status");
        super::track_task(&mut t, "TaskUpdate", &json!({"taskId": 2, "status": "deleted"}), "ok");
        assert_eq!(t, [("1".into(), "a".into(), "in_progress".into())] as [(String, String, String); 1]);
    }
}

/// A shell command Claude Code tracks as a task.
#[derive(Clone, Debug)]
pub struct Shell {
    pub task_id: String,
    pub tool_use_id: String,
    pub desc: String,
    pub background: bool,
    /// `None` while it runs, then Claude Code's word for how it ended (completed, failed, stopped...).
    pub status: Option<String>,
    pub summary: String,
}
