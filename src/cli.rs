//! Command line: the usage text, the options, and the profile names they accept.

use super::*;

pub(crate) const HELP: &str = "\
claude++ — a lighter native frontend for headless Claude Code

USAGE: claude++ [options] [-- <extra claude args>]
       claude++ procs        the Claude-related processes on this machine, who owns each, what it holds (read-only)
       claude++ supervisor   the process every claude++ shares; started for you, ends when the last one has left

  --profile <name>            profile for new tabs: full|sources|lean or one of yours (see PROFILES) (change per tab with /profile, /new <profile>)
                        sources = project+local settings only; lean = that plus core tools, no MCP
  --env KEY=VAL         set an env var on every claude process (repeatable), e.g.
                        BUN_JSC_gcMaxHeapSize=100000000
  --model <m>           model alias or id
  --permission-mode <m> default | acceptEdits | plan | auto | ...
  -c, --continue        continue the latest conversation in this directory
  -r, --resume <id>     resume a session by id
  --idle-suspend <s>    stop idle background tabs' claude after s seconds (0 = never, default 120);
                        the supervisor holds one value for all windows, and this sets it
  --mem-budget <MB>     when all claude processes of all claude++ windows exceed this footprint, stop the
                        least recently used idle tab (0 = off, default). Busy/focused tabs are never
                        stopped. One value for all windows, like --idle-suspend: see README
  --cache-ttl <min>     prompt-cache lifetime used for the status-line countdown (default 60; use 5
                        if your account only gets the short cache)
  --statusline <v>      which status line to show: `claude` runs the `statusLine` command from your Claude Code
                        settings (rich, but a process per refresh), `builtin` is claude++'s own line only (nothing
                        spawned), or a shell command to run instead. Default: what /statusline last saved in
                        ~/.config/claude++/config.json, else builtin
  --claude-bin <path>   claude executable (default: claude)

KEYS  enter send · shift+enter, alt+enter or ctrl+j newline · esc/ctrl+c interrupt
      esc esc on an empty input: rewind to an earlier message (conversation, files, or both)
      ctrl+t new tab · ctrl+n/ctrl+p next/prev tab · alt+1..9 jump · ctrl+d close tab
      shift+tab permission mode · ctrl+o expand all tool output · alt+o folds one call · pgup/pgdn scroll · ctrl+end jump to latest
      ctrl+g edit the message in your editor, full screen inside claude++ (ctrl+\\ cancels) · ctrl+b move a running command to the background (/tasks stops it)
      alt+q manage queued messages (reorder, send now, edit) · ctrl+f search the conversation or what you sent
      /editor: the message area is your editor, always open (quit it to send; ctrl+\\ cancels; no / or @ completion).
              Still claude++'s: pgup/pgdn, alt+1..9, shift+tab, ctrl+t/o, alt+f/r/d/b (as ctrl+f/r/d/b), alt+←/→ tabs, and until you type: ↑↓ history, ← sessions, esc
      /vim: modal keys in the box (esc normal mode; i a o insert; hjkl w b e f t d c y p u v; / searches); /config → Input mode
      ctrl+r search the messages you sent · ← at the start of the input: earlier sessions · tab completes a /command or an @file (↑↓ to pick, esc hides)
      ctrl+v paste an image as [Image #N] at the cursor (delete the token to drop it)
COMMANDS  /usage (/cost /status) /memory [user] /export [file] /tasks /context /queue /schedule /search /vim /find /session /new [profile] /profile [name] [--hard] /close /clear /resume [id] /model [m] /effort [level] /mode <m>
          /config (/setting) /statusline [claude|builtin|items] /img <path> /quit
          anything else starting with / goes to Claude Code (e.g. /compact)
          !<command> runs a shell command here (no model turn); its output goes with your next message
@FILES    type @ to pick a file or folder; Claude Code reads what you mention (images included)
PROFILES  your own: ~/.config/claude++/profiles/<name>.json (system prompt, tools, MCP, model, env...; see README)
EDITING   alt+←/→ word · alt+backspace / ctrl+w delete word · ctrl+u / ctrl+k cut to line start / end · ↑↓ move between rows, then history · ctrl+z undo, ctrl+shift+z or alt+z redo · alt+p model picker · ctrl+l repaint · /copy [code] copies the last answer
IMAGES    ctrl+v, drag a file into the window or /img <path> to attach; pictures in the transcript are drawn
          inline in kitty/Ghostty (CLAUDEPP_IMAGES=on|off overrides detection)";

pub(crate) struct Opts {
    pub(crate) launch: Launch,
    pub(crate) resume: Option<String>,
    pub(crate) cont: bool,
    pub(crate) idle: Option<u64>,
    pub(crate) mem_budget_mb: Option<u64>,
    pub(crate) profile: String,
    pub(crate) cache_ttl_min: Option<u64>,
    pub(crate) statusline: String,
}

/// `--profile lean` = this tool set, no MCP, and no user-level settings (user CLAUDE.md,
/// skills, hooks). Measured on Claude Code 2.1.309 in this repo: 65k → 7k prompt tokens.
pub(crate) fn profile_args(p: &str) -> Result<Vec<String>, String> {
    profile::spec(p).map(|s| s.args)
}

/// `profile` from `<dir>/.claude++/config.json`, if the project has one and it names a profile that exists.
pub(crate) fn project_profile(dir: &str) -> Option<String> {
    let file = std::path::Path::new(dir).join(".claude++").join("config.json");
    config::get_str(&file, "profile").filter(|p| profile_args(p).is_ok())
}

pub(crate) fn parse_args() -> Result<Opts, String> {
    let mut a = std::env::args().skip(1);
    let mut o = Opts {
        // Headless Claude Code keeps no file checkpoints unless asked; a rewind restores code from them.
        launch: Launch { bin: "claude".into(), env: vec![("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".into(), "1".into())], ..Default::default() },
        resume: None,
        cont: false,
        idle: None,
        mem_budget_mb: None,
        profile: String::new(),
        cache_ttl_min: None,
        statusline: String::new(),
    };
    let mut profile = String::new(); // empty: not given, so the saved or project choice applies
    let mut extra = vec![];
    let need = |a: &mut std::iter::Skip<std::env::Args>, f: &str| a.next().ok_or(format!("{f} needs a value"));
    while let Some(arg) = a.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(HELP.into()),
            "--profile" => profile = need(&mut a, &arg)?,
            "--env" => {
                let kv = need(&mut a, &arg)?;
                let (k, v) = kv.split_once('=').ok_or("--env needs KEY=VALUE")?;
                o.launch.env.push((k.into(), v.into()));
            }
            "--model" | "--permission-mode" => {
                extra.push(arg.clone());
                extra.push(need(&mut a, &arg)?);
            }
            "-c" | "--continue" => o.cont = true,
            "-r" | "--resume" => o.resume = Some(need(&mut a, &arg)?),
            "--idle-suspend" => o.idle = Some(need(&mut a, &arg)?.parse().map_err(|_| "bad --idle-suspend")?),
            "--mem-budget" => o.mem_budget_mb = Some(need(&mut a, &arg)?.parse().map_err(|_| "bad --mem-budget")?),
            "--cache-ttl" => o.cache_ttl_min = Some(need(&mut a, &arg)?.parse().map_err(|_| "bad --cache-ttl")?),
            "--statusline" => o.statusline = need(&mut a, &arg)?,
            "--claude-bin" => o.launch.bin = need(&mut a, &arg)?,
            "--" => extra.extend(a.by_ref()),
            other => return Err(format!("unknown option {other}\n\n{HELP}")),
        }
    }
    profile_args(if profile.is_empty() { "full" } else { &profile })?; // validate; each tab applies its own profile at launch
    // Bypass joins the shift+tab cycle when the backend is allowed to bypass (flags after `--`).
    let bypass = extra.iter().any(|a| a.contains("dangerously-skip-permissions")) || extra.windows(2).any(|w| w[0] == "--permission-mode" && w[1] == "bypassPermissions");
    tabcfg::BYPASS.store(bypass, std::sync::atomic::Ordering::Relaxed);
    o.profile = profile;
    o.launch.args = extra;
    Ok(o)
}
