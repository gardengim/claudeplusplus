//! App-level tests: key routing, panels, commands, tabs. Unit tests for the other modules sit beside them.

use super::*;
use crate::backend::Resume;
use crate::tty::edit_text;
use serde_json::{Value, json};

fn run_cmd(app: &mut App, cmd: &str) {
    app.input = cmd.into();
    app.cursor = cmd.chars().count();
    app.submit();
}

#[test]
fn profiles_apply_per_tab_and_model_switch_warns_about_recache() {
    let (mut app, dir) = fake_app("prof");
    app.on_key(key('h')); // tab 0, default profile: spawns with no profile flags
    run_cmd(&mut app, "/new lean");
    assert_eq!(app.sessions[1].profile, "lean");
    app.on_key(key('x')); // tab 1: lean flags
    wait_spawns(&dir, 2);
    let log = std::fs::read_to_string(dir.join("spawns")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2);
    // The two scripts append concurrently, so don't depend on line order.
    let lean: Vec<_> = lines.iter().filter(|l| l.contains("--tools")).collect();
    assert_eq!(lean.len(), 1, "{lines:?}");
    assert!(lean[0].contains("--setting-sources project,local"), "{}", lean[0]);

    // Mid-conversation switch: backend is dropped (next message resumes with new flags) and the note
    // says what it costs; refused while a turn is running.
    app.sessions[1].ctx_tokens = 12_000;
    run_cmd(&mut app, "/profile full");
    assert!(app.sessions[1].backend.is_none() && app.sessions[1].profile == "full");
    let last = app.sessions[1].items.last().unwrap();
    assert!(matches!(&last.entry, session::Entry::Note(t) if t.contains("keeps the system prompt")));
    assert!(!app.launch_for(1).args.contains(&"--system-prompt-snapshot".to_string()));
    run_cmd(&mut app, "/profile lean --hard");
    assert!(app.sessions[1].unpinned && app.sessions[1].profile == "lean");
    let a = app.launch_for(1).args;
    assert!(a.windows(2).any(|w| w == ["--system-prompt-snapshot", "off"]), "{a:?}");
    let last = app.sessions[1].items.last().unwrap();
    assert!(matches!(&last.entry, session::Entry::Note(t) if t.contains("re-caches up to ~12k")));
    run_cmd(&mut app, "/model sonnet");
    let last = app.sessions[1].items.last().unwrap();
    assert!(matches!(&last.entry, session::Entry::Note(t) if t.contains("re-caches ~12k")), "model switch warns");
    app.sessions[1].busy = true;
    run_cmd(&mut app, "/profile sources");
    assert_eq!(app.sessions[1].profile, "lean");
    assert!(app.flash.is_some());
    run_cmd(&mut app, "/new bogus");
    assert_eq!(app.sessions.len(), 2, "unknown profile opens nothing");
}

#[test]
fn cache_clock_counts_down_from_the_last_api_response() {
    let mut s = Session::new(1, String::new());
    let ttl = Duration::from_secs(3600);
    assert!(s.cache_left(ttl).is_none(), "no conversation, no cache");
    s.ctx_tokens = 5000;
    s.last_api = Instant::now().checked_sub(Duration::from_secs(600));
    let left = s.cache_left(ttl).unwrap();
    assert!(left > Duration::from_secs(2900) && left <= Duration::from_secs(3000));
    s.last_api = Instant::now().checked_sub(Duration::from_secs(4000));
    assert!(s.cache_left(ttl).unwrap().is_zero());
}

#[test]
fn footprint_output_is_parsed() {
    let out = "======\nclaude [123]: 64-bit    Footprint: 1.5 GB (16384 bytes per page)\n======\nnode [45]: 64-bit    Footprint: 85 MB (16384 bytes per page)\nsleep [6]: 64-bit    Footprint: 1056 KB (16384 bytes per page)\n  Dirty   Clean\n";
    assert_eq!(memory::parse_footprint(out), vec![(123, 1572864), (45, 87040), (6, 1056)]);
}

#[test]
fn leaving_a_tab_restarts_its_idle_clock_so_it_is_not_stopped_the_moment_you_leave() {
    let (mut app, dir) = fake_app("leave");
    app.new_tab(); // tabs 0,1; focus on 1
    app.sessions[1].last_active = Instant::now() - Duration::from_secs(600);
    app.goto(0);
    assert!(app.sessions[1].last_active.elapsed() < Duration::from_secs(5), "idle time counts from when you left, not from the last message");
    app.sessions[0].last_active = Instant::now() - Duration::from_secs(600);
    app.lanes = Some(Default::default());
    app.new_tab(); // opening a tab leaves the front one too
    assert!(app.sessions[0].last_active.elapsed() < Duration::from_secs(5) && app.lanes.is_none(), "and its /tasks list stays behind with it");
    let _ = dir;
}

#[test]
fn a_backend_nobody_sent_to_may_be_stopped_but_not_one_that_was_until_it_has_a_session_id() {
    let (mut app, _d) = fake_app("unused");
    let (launch, tx) = (app.launch.clone(), app.tx.clone());
    app.sessions[0].warm(&launch, &tx);
    assert!(app.sessions[0].backend.is_some() && app.sessions[0].can_suspend(), "warmed and never used: nothing to lose");
    app.sessions[0].submit("hi".into(), &[], &launch, &tx);
    app.sessions[0].busy = false; // the turn ended without an init event, so there is still no session id
    assert!(app.sessions[0].session_id.is_none() && !app.sessions[0].can_suspend(), "a message went to it: stopping would lose the conversation");
}

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn fake_app(tag: &str) -> (App, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-warm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> {}/spawns\nwhile read l; do :; done\n", dir.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let launch = Launch { bin: script.to_string_lossy().into(), ..Default::default() };
    let (tx, _rx) = mpsc::channel();
    std::mem::forget(_rx); // keep the channel open for reader threads
    let mut app = App::new(launch, "/".into(), tx);
    app.new_tab();
    (app, dir)
}

fn count(dir: &std::path::Path) -> usize {
    std::fs::read_to_string(dir.join("spawns")).map_or(0, |s| s.lines().count())
}

/// Wait (up to 3 s) for `n` spawns to be logged, so a loaded machine cannot fail the test.
fn wait_spawns(dir: &std::path::Path, n: usize) -> usize {
    let end = Instant::now() + Duration::from_secs(3);
    while count(dir) < n && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    count(dir)
}

/// For asserting that nothing spawned: there is nothing to wait for, so allow a grace period.
fn spawns(dir: &std::path::Path) -> usize {
    thread::sleep(Duration::from_millis(300));
    count(dir)
}

#[test]
fn backend_warms_on_first_keystroke_once_and_never_for_slash_commands() {
    let (mut app, dir) = fake_app("a");
    assert_eq!(spawns(&dir), 0, "idle UI spawns nothing");
    for c in "hello".chars() {
        app.on_key(key(c));
    }
    assert_eq!(wait_spawns(&dir, 1), 1, "typing spawns one backend, not one per key");
    assert!(app.sessions[0].items.is_empty(), "warming sends no message");

    let (mut app2, dir2) = fake_app("b");
    app2.known_commands = vec![cmd("compact", "", "summarize")];
    for c in "/new".chars() {
        app2.on_key(key(c));
    }
    assert_eq!(spawns(&dir2), 0, "slash commands do not spawn once their list is known");

    let (mut app3, dir3) = fake_app("c");
    app3.on_key(key('/'));
    assert_eq!(wait_spawns(&dir3, 1), 1, "the very first `/` starts a backend to learn the command list");
}

fn cmd(name: &str, hint: &str, desc: &str) -> Cmd {
    Cmd { name: name.into(), hint: hint.into(), desc: desc.into() }
}
fn typed(app: &mut App, text: &str) {
    text.chars().for_each(|c| app.on_key(key(c)));
}
fn names(app: &App) -> Vec<String> {
    app.completions().into_iter().map(|c| c.name).collect()
}
fn press(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn completion_ranks_prefix_first_and_dedupes_local_commands() {
    let (mut app, _d) = fake_app("comp");
    app.known_commands = vec![cmd("compact", "[focus]", "summarize"), cmd("model", "", "theirs"), cmd("context", "", "c")];
    typed(&mut app, "/c");
    assert_eq!(names(&app), ["cd", "clear", "close", "color", "config", "context", "copy", "compact", "instructions", "procs", "schedule", "search"]);
    press(&mut app, KeyCode::Backspace);
    typed(&mut app, "m");
    // prefix matches ("memory", "mode", "model") before names that merely contain "m"; ours before theirs
    assert_eq!(names(&app), ["memory", "mode", "model", "img", "optimize", "permissions", "resume", "swarm", "term", "vim", "compact"]);
    let model = app.completions().into_iter().find(|c| c.name == "model").unwrap();
    assert_eq!(model.desc, "pick a model and effort, or switch directly", "ours wins over Claude Code's same-named command");
    assert!(names(&app).iter().filter(|n| *n == "model").count() == 1);
}

#[test]
fn tab_and_enter_complete_a_partial_name_but_enter_runs_a_complete_one() {
    let (mut app, dir) = fake_app("tab");
    app.known_commands = vec![cmd("compact", "[focus]", "summarize")];
    typed(&mut app, "/comp");
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "/compact ");
    assert!(app.completions().is_empty(), "popup closes once an argument may follow");

    app.set_input("/co".into());
    press(&mut app, KeyCode::Down);
    let picked = app.completions()[app.sel].name.clone();
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.input, format!("/{picked} "), "Enter on a partial name completes instead of sending");
    assert!(app.sessions[0].items.is_empty());
    assert_eq!(spawns(&dir), 0);

    // A complete, exact name runs. /close on the only tab ends the app.
    app.set_input("/close".into());
    press(&mut app, KeyCode::Enter);
    assert!(app.quit);
}

#[test]
fn no_popup_for_plain_text_arguments_or_a_pending_prompt() {
    let (mut app, _d) = fake_app("nopop");
    typed(&mut app, "hello /c");
    assert!(app.completions().is_empty());
    app.set_input("/model so".into());
    assert!(app.completions().is_empty());
}

#[test]
fn clear_and_img_are_handled_locally() {
    let (mut app, dir) = fake_app("local");
    app.sessions[0].push(session::Entry::User("old".into()));
    run_cmd(&mut app, "/clear");
    assert!(app.sessions[0].items.is_empty());
    run_cmd(&mut app, "/img");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("usage")));
    run_cmd(&mut app, "/img /definitely/not/here.png");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("here.png")), "{:?}", app.flash);
    assert_eq!(spawns(&dir), 0, "none of these reach Claude Code");
    assert!(app.sessions[0].items.iter().all(|i| !matches!(i.entry, session::Entry::User(_))));
}

#[test]
fn command_list_is_cached_on_disk() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-cmds-{}", std::process::id()));
    let path = dir.join("sub").join("commands.json");
    assert!(read_commands(&path).is_empty());
    write_commands(&path, &[cmd("compact", "[focus]", "summarize"), cmd("x", "", "")]);
    let back = read_commands(&path);
    assert_eq!((back.len(), back[0].name.as_str(), back[0].hint.as_str(), back[0].desc.as_str()), (2, "compact", "[focus]", "summarize"));
}

fn project(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-proj-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for f in ["src/main.rs", "src/session.rs", "docs/pic.png", "README.md"] {
        std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
        std::fs::write(dir.join(f), "x").unwrap();
    }
    dir
}

#[test]
fn at_picker_completes_files_and_folders_and_hides_on_esc() {
    let (mut app, _d) = fake_app("at");
    app.cwd = project("at").to_string_lossy().into();
    typed(&mut app, "look at @sma");
    assert_eq!(names(&app)[0], "src/main.rs");
    assert!(app.completions().iter().all(|c| c.prefix == '@'));
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.input, "look at @src/main.rs ");
    assert!(app.completions().is_empty(), "a finished word closes the popup");

    app.set_input("@".into());
    app.refresh_files();
    assert_eq!(names(&app), ["docs/", "src/", "README.md"], "bare @ lists the top level");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter); // src/ : a folder keeps the popup open
    assert_eq!(app.input, "@src/");
    assert!(!app.completions().is_empty());
    assert!(app.sessions[0].items.is_empty(), "Enter picked a file, it did not send");
    typed(&mut app, "ses");
    assert_eq!(names(&app)[0], "src/session.rs");

    press(&mut app, KeyCode::Esc);
    assert!(app.completions().is_empty(), "esc hides the popup");
    typed(&mut app, "s");
    assert!(!app.completions().is_empty(), "typing brings it back");
    app.set_input("mail me@src".into());
    app.refresh_files();
    assert!(app.completions().is_empty(), "an @ inside a word is not a mention");
}

#[test]
fn image_mentions_are_found_relative_to_the_project() {
    let (mut app, _d) = fake_app("mention");
    app.cwd = project("mention").to_string_lossy().into();
    let found = app.mentioned_images("see @docs/pic.png, and @src/main.rs and @missing.png and @docs/pic.png.");
    assert_eq!(found, vec![std::path::PathBuf::from(&app.cwd).join("docs/pic.png")]);
}

fn paste(app: &mut App) {
    app.attach_bytes("clip.png".into(), vec![1, 2, 3], "image/png");
}
fn visible_input(app: &App) -> String {
    app.input.replace('\n', "|")
}

#[test]
fn a_pasted_image_becomes_a_token_on_its_own_line_at_the_cursor() {
    let (mut app, _d) = fake_app("tok");
    typed(&mut app, "this is text sample");
    paste(&mut app);
    assert_eq!(visible_input(&app), "this is text sample|[Image #1]|");
    typed(&mut app, "And this is pasted image.");
    assert_eq!(visible_input(&app), "this is text sample|[Image #1]|And this is pasted image.");

    // pasting in the middle of a line splits it; a second paste gets the next number
    app.set_input("before after".into());
    app.cursor = 7;
    paste(&mut app);
    assert_eq!(visible_input(&app), "before |[Image #2]|after");
    assert_eq!(app.sessions[0].attachments.iter().map(|a| a.n).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(app.input.chars().nth(app.cursor), Some('a'), "cursor lands after the token");
}

#[test]
fn deleting_a_token_removes_it_whole_and_undo_brings_its_picture_back() {
    let (mut app, _d) = fake_app("deltok");
    typed(&mut app, "x");
    paste(&mut app);
    paste(&mut app);
    assert_eq!(visible_input(&app), "x|[Image #1]|[Image #2]|", "no blank line is added after a newline");
    // cursor is after "[Image #2]\n": step back over the newline, then one backspace takes the token
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(visible_input(&app), "x|[Image #1]|");
    for _ in 0..2 {
        app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)); // each backspace was its own step
    }
    assert_eq!(visible_input(&app), "x|[Image #1]|[Image #2]|", "ctrl+z brings the token back…");
    assert_eq!(app.sessions[0].attachments.iter().map(|a| a.n).collect::<Vec<_>>(), [1, 2], "…and its picture with it");
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    paste(&mut app);
    assert!(app.input.contains("[Image #3]"), "a number ctrl+z could restore is never reused");
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.sessions[0].attachments.iter().map(|a| a.n).collect::<Vec<_>>(), [1, 2, 3], "kept while ctrl+z can restore them");
    app.undo.clear();
    app.drop_orphan_images("");
    assert_eq!(app.sessions[0].attachments.iter().map(|a| a.n).collect::<Vec<_>>(), [1], "with nothing to undo, a deleted token frees its picture");
    // Delete in front of a token removes it whole too
    app.cursor = 2;
    press(&mut app, KeyCode::Delete);
    assert_eq!(visible_input(&app), "x||");
    assert_eq!(app.sessions[0].attachments.len(), 1, "still undoable");
    app.undo.clear();
    app.drop_orphan_images("");
    assert!(app.sessions[0].attachments.is_empty());
    // a half-typed token is just text
    app.set_input("[Image #1".into());
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.input, "[Image #");
}

#[test]
fn a_clipboard_image_read_off_thread_lands_in_its_own_tab_or_nowhere() {
    let (mut app, _d) = fake_app("clip");
    let first = app.sessions[0].id;
    app.on_event(AppEvent::Clipboard(first, None, "no image"));
    assert!(app.sessions[0].attachments.is_empty());
    app.on_event(AppEvent::Clipboard(first, Some(vec![1, 2, 3]), "no image"));
    assert_eq!(visible_input(&app), "[Image #1]|");
    app.new_tab();
    app.on_event(AppEvent::Clipboard(first, Some(vec![1, 2, 3]), "no image"));
    assert!(app.sessions[app.active].attachments.is_empty() && app.input.is_empty(), "not into the tab that happens to be in front");
}

#[test]
fn the_message_is_sent_with_text_and_pictures_in_the_order_written() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let launch = Launch { bin: script.to_string_lossy().into(), ..Default::default() };
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(launch, "/".into(), tx);
    app.new_tab();

    typed(&mut app, "this is text sample");
    paste(&mut app);
    typed(&mut app, "And this is pasted image.");
    app.set_input(format!("{}\nIt should have preview", app.input));
    app.attach_bytes("gone.png".into(), vec![9], "image/png"); // pasted, then its token deleted below
    let stale = image::token(2);
    app.input = app.input.replace(&stale, "");
    app.drop_orphan_images("");
    press(&mut app, KeyCode::Enter);
    drop(rx);

    let log = (0..60)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::read_to_string(dir.join("stdin")).ok().filter(|t| t.contains("\"user\""))
        })
        .expect("the user message reached the process");
    let msg = log.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).find(|v| v["type"] == "user").unwrap();
    let kinds: Vec<String> = msg["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| if b["type"] == "text" { format!("text:{}", b["text"].as_str().unwrap().replace('\n', "|")) } else { b["type"].as_str().unwrap().to_string() })
        .collect();
    assert_eq!(kinds, ["text:this is text sample", "text:[Image #1]", "image", "text:And this is pasted image.|It should have preview"]);
    assert!(matches!(&app.sessions[0].items[0].entry, session::Entry::Rich(p) if p.len() == 3));
    assert_eq!(app.sessions[0].title, "this is text sample".chars().take(18).collect::<String>());
    assert!(app.sessions[0].attachments.is_empty());
    assert_eq!(app.history, ["this is text sample\nAnd this is pasted image.\nIt should have preview"], "history keeps words, not tokens");
}

#[test]
fn a_bang_command_runs_here_without_a_turn_and_its_output_goes_with_the_next_message() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-bang-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let launch = Launch { bin: script.to_string_lossy().into(), ..Default::default() };
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(launch, dir.to_string_lossy().into(), tx);
    app.new_tab();

    typed(&mut app, "!echo hi; pwd");
    press(&mut app, KeyCode::Enter);
    let ev = (0..100).find_map(|_| match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(e @ AppEvent::Shell(..)) => Some(e),
        _ => None,
    });
    app.on_event(ev.expect("the command finished"));
    let sent = std::fs::read_to_string(dir.join("stdin")).unwrap_or_default();
    assert!(!sent.contains("\"user\""), "no model turn for a ! command");
    let shown = match &app.sessions[0].items[0].entry {
        session::Entry::Tool { name, result: Some((r, false)), .. } if name == "Bash" => r.clone(),
        _ => panic!("shown as a finished Bash call"),
    };
    assert!(shown.starts_with("hi\n") && shown.contains("claudeplusplus-bang-"), "it ran in the tab's folder: {shown}");

    typed(&mut app, "/compact");
    press(&mut app, KeyCode::Enter);
    app.sessions[0].busy = false; // the /compact turn is over: a message now goes straight out
    typed(&mut app, "what did it print?");
    press(&mut app, KeyCode::Enter);
    let log = (0..60)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::read_to_string(dir.join("stdin")).ok().filter(|t| t.matches("\"user\"").count() >= 2)
        })
        .expect("both messages reached the process");
    let users: Vec<serde_json::Value> = log.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).filter(|v| v["type"] == "user").collect();
    assert_eq!(users[0]["message"]["content"], "/compact", "a command stays a command; the output waits");
    let msg = &users[1];
    let text = msg["message"]["content"].as_str().unwrap();
    assert!(text.starts_with("<bash-input>echo hi; pwd</bash-input>\n<bash-stdout>hi\n") && text.ends_with("what did it print?"), "{text}");
    assert_eq!(history::user_text(msg).as_deref(), Some("what did it print?"), "a resumed transcript shows only the words typed");
}


#[test]
fn previews_are_drawn_under_their_token_in_the_input_and_in_the_transcript() {
    use ratatui::{Terminal, backend::TestBackend, layout::Position};
    let (mut app, _d) = fake_app("prev");
    let shown = image::Shown { id: 0x0001_0203, cols: 6, rows: 3, w: 10, h: 5 };
    typed(&mut app, "look");
    paste(&mut app);
    typed(&mut app, "done");
    app.sessions[0].attachments[0].preview = Some(shown);

    let render = |app: &mut App| -> (Vec<String>, Position) {
        let mut t = Terminal::new(TestBackend::new(40, 24)).unwrap();
        t.draw(|f| ui::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        let rows = (0..24).map(|y| (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect();
        (rows, t.get_cursor_position().unwrap())
    };
    let (rows, cursor) = render(&mut app);
    let at = rows.iter().position(|r| r.contains("[Image #1]")).expect("token line");
    assert!(rows[at + 1..=at + 3].iter().all(|r| r.contains('\u{10EEEE}')), "three preview rows follow the token: {rows:?}");
    assert!(rows[at + 4].contains("done"), "the text after the picture comes after its rows");
    assert_eq!(cursor.y as usize, at + 4, "cursor is on the text row below the picture");

    // after sending, the transcript holds the same order
    app.sessions[0].push(session::Entry::Rich(vec![
        session::Part::Text("look".into()),
        session::Part::Image(1, Some(shown)),
        session::Part::Text("done".into()),
    ]));
    app.set_input(String::new());
    let (rows, _) = render(&mut app);
    let look = rows.iter().position(|r| r.contains("❯ look")).unwrap();
    assert!(rows[look + 1].contains("[Image #1]") && rows[look + 2].contains('\u{10EEEE}') && rows[look + 4].contains('\u{10EEEE}'));
    assert!(rows[look + 5].contains("done"));
}

fn model(value: &str, resolved: &str, efforts: &[&str]) -> ModelInfo {
    // only the opus models have fast mode, as in real life
    ModelInfo { value: value.into(), name: value.to_uppercase(), desc: String::new(), resolved: resolved.into(), efforts: efforts.iter().map(|e| e.to_string()).collect(), fast: value.starts_with("opus") }
}

#[test]
fn the_picker_has_four_menus_and_only_the_one_in_focus_answers_the_arrows() {
    let (mut app, _dir) = fake_app("four-menus");
    let named = |value: &str, name: &str, efforts: &[&str]| ModelInfo { name: name.into(), desc: format!("desc-{value}"), ..model(value, value, efforts) };
    app.known_models = vec![
        named("default", "Default (recommended)", &["low", "high"]),
        named("sonnet", "Sonnet 5.5", &["low", "high"]),
        named("haiku", "Haiku 5.5", &["low", "medium", "high"]),
        named("s5", "Sonnet 5", &["low"]),
        named("h45", "Haiku 4.5", &[]),
    ];
    app.model_picker = Some(ModelPicker { sel: 1, effort: 1, fast: false, focus: 0 });
    let view = |app: &App| {
        let v = ui::model_view(app);
        (v.body.iter().map(|l| l.to_string()).collect::<Vec<_>>(), v.tail.iter().map(|l| l.to_string()).collect::<Vec<_>>())
    };
    let sel = |app: &App| app.model_picker.unwrap().sel;
    let (rows, tail) = view(&app);
    assert!(rows[1].contains("Sonnet") && rows[2].contains("Haiku") && rows[0].contains("Default"), "one row per family: {rows:?}");
    assert!(rows[0].contains(" 5.5 ") && rows[1].contains(" 5 "), "the versions of the highlighted family, in a column beside it: {rows:?}");
    assert!(tail[0].contains("desc-sonnet"), "{tail:?}");

    // Models have the keys: only up and down act, and only on the models.
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Left);
    assert_eq!((sel(&app), app.model_picker.unwrap().effort), (1, 1), "left and right do nothing here");
    press(&mut app, KeyCode::Down); // Haiku, on the same version, 5.5
    assert_eq!(sel(&app), 2);
    press(&mut app, KeyCode::Up);
    assert_eq!(sel(&app), 1);

    // Tab: versions. Up and down move the versions only.
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.model_picker.unwrap().focus, 1);
    press(&mut app, KeyCode::Down);
    assert_eq!(sel(&app), 3, "Sonnet 5");
    press(&mut app, KeyCode::Down);
    assert_eq!(sel(&app), 1, "wraps, like every short list");
    press(&mut app, KeyCode::Left);
    assert_eq!(sel(&app), 1, "left does nothing in a vertical menu");

    // Tab: effort. Left and right move it; up and down do nothing.
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.model_picker.unwrap().focus, 2);
    press(&mut app, KeyCode::Down);
    assert_eq!(sel(&app), 1);
    assert_eq!(app.model_picker.unwrap().effort, 0);
    press(&mut app, KeyCode::Left);
    assert_eq!(app.model_picker.unwrap().effort, 1, "wraps, like every short row of choices");
    press(&mut app, KeyCode::Tab); // Sonnet has no fast mode: round to the models
    assert_eq!(app.model_picker.unwrap().focus, 0);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.model_picker.unwrap().focus, 2, "shift+tab goes the other way");

    // A family with a single entry has no versions menu to stop at.
    app.model_picker = Some(ModelPicker { sel: 0, effort: 0, fast: false, focus: 1 });
    press(&mut app, KeyCode::Down);
    assert_eq!(sel(&app), 1, "focus was on an unusable menu: the models have the keys, and Down moves them");
}

#[test]
fn model_picker_applies_model_and_effort_and_remembers_them() {
    let (mut app, dir) = fake_app("picker");
    app.known_models = vec![model("sonnet", "claude-sonnet-5-5", &["low", "high"]), model("haiku", "claude-haiku-5-5", &["low", "medium", "high"])];
    app.sessions[0].model = "claude-sonnet-5-5".into();
    app.sessions[0].effort = "high".into();
    run_cmd(&mut app, "/model");
    let p = app.model_picker.unwrap();
    assert_eq!((p.sel, p.effort), (0, 1), "opens on the current model and effort");
    typed(&mut app, "zzz");
    assert!(app.input.is_empty(), "keys go to the picker");

    press(&mut app, KeyCode::Down); // haiku: "high" is kept by name (index 2)
    assert_eq!(app.model_picker.unwrap().effort, 2);
    press(&mut app, KeyCode::Left);
    assert_eq!(app.model_picker.unwrap().effort, 2, "the models have the keys: left does nothing");
    press(&mut app, KeyCode::Tab); // the effort menu has them now (there are no versions to stop at)
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Enter);
    assert!(app.model_picker.is_none());
    assert_eq!(app.sessions[0].model_pref.as_deref(), Some("haiku"));
    assert_eq!(app.sessions[0].effort_pref.as_deref(), Some("medium"));
    assert_eq!(spawns(&dir), 0, "choosing before the backend exists starts nothing");

    // Escape changes nothing; choosing what is already active is a no-op.
    run_cmd(&mut app, "/effort");
    press(&mut app, KeyCode::Esc);
    assert!(app.model_picker.is_none());
    run_cmd(&mut app, "/effort bogus");
    assert_eq!(app.sessions[0].effort_pref.as_deref(), Some("medium"), "a level the model lacks is refused");
}

#[test]
fn current_model_prefers_the_pick_and_picker_fits_a_short_terminal() {
    let (mut app, _d) = fake_app("cur");
    app.known_models = vec![model("default", "claude-sonnet-5-5", &["low", "high"]), model("sonnet", "claude-sonnet-5-5", &["low"]), model("haiku", "claude-haiku-5-5", &["low"])];
    assert_eq!(app.current_model(), None);
    app.sessions[0].model = "claude-sonnet-5-5".into();
    assert_eq!(app.current_model(), Some(0), "first alias with that id");
    app.sessions[0].model_pref = Some("sonnet".into());
    assert_eq!(app.current_model(), Some(1), "the explicit pick wins");

    app.known_models.extend((0..20).map(|i| model(&format!("m{i}"), &format!("id{i}"), &["low", "high"])));
    run_cmd(&mut app, "/model");
    app.model_picker = Some(ModelPicker { sel: 22, effort: 1, fast: false, focus: 0 });
    let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 16)).unwrap();
    t.draw(|f| ui::draw(f, &mut app)).unwrap();
    let text: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("M19") && text.contains("effort") && text.contains("⏎ apply"), "selected row and effort row both visible");
    assert!(text.contains("fast") && text.contains("not available"), "the fast row is there even for a model without it");
}

#[test]
fn fast_mode_toggles_in_the_picker_for_opus_only_and_follows_model_switches() {
    let (mut app, _dir) = fake_app("fast");
    app.known_models = vec![model("sonnet", "claude-sonnet-5-5", &["low", "high"]), model("opus", "claude-opus-5-5", &["low", "high"])];
    app.sessions[0].model = "claude-sonnet-5-5".into();
    app.sessions[0].effort = "high".into();
    run_cmd(&mut app, "/model");
    press(&mut app, KeyCode::Char(' '));
    assert!(!app.model_picker.unwrap().fast, "sonnet has no fast mode: space does nothing");
    press(&mut app, KeyCode::Down); // opus
    press(&mut app, KeyCode::Char(' '));
    assert!(!app.model_picker.unwrap().fast, "the models have the keys, not fast mode");
    press(&mut app, KeyCode::Tab); // effort
    press(&mut app, KeyCode::Tab); // fast mode
    press(&mut app, KeyCode::Char(' '));
    assert!(app.model_picker.unwrap().fast);
    press(&mut app, KeyCode::Char('f'));
    assert!(!app.model_picker.unwrap().fast, "f toggles too");
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Enter);
    let s = &app.sessions[0];
    assert_eq!((s.model_pref.as_deref(), s.fast_pref), (Some("opus"), Some(true)));
    assert!(matches!(&s.items.last().unwrap().entry, session::Entry::Note(t) if t.contains("fast ↯") && t.contains("usage credits")), "the cost is stated");

    // the backend confirms: the state follows what it reports
    app.sessions[0].handle(&serde_json::from_str(r#"{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{"fast_mode_state":"on"}}}"#).unwrap());
    assert!(app.sessions[0].fast_mode);

    // moving to a model without fast mode turns it off; the picker opens showing the current state
    run_cmd(&mut app, "/model");
    assert!(app.model_picker.unwrap().fast, "opens on the current fast state");
    press(&mut app, KeyCode::Up); // sonnet
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.sessions[0].fast_pref, Some(false));
    assert_eq!(app.sessions[0].model_pref.as_deref(), Some("sonnet"));
}

#[test]
fn a_direct_model_switch_away_from_opus_turns_fast_mode_off() {
    let (mut app, _dir) = fake_app("fast2");
    app.sessions[0].models = vec![model("sonnet", "claude-sonnet-5-5", &["low"]), model("opus", "claude-opus-5-5", &["low"])];
    app.sessions[0].fast_mode = true;
    run_cmd(&mut app, "/model opus");
    assert_eq!(app.sessions[0].fast_pref, None, "still opus: untouched");
    run_cmd(&mut app, "/model sonnet");
    assert_eq!(app.sessions[0].fast_pref, Some(false));
}

#[test]
fn the_suggestion_popup_uses_the_whole_screen_width() {
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _d) = fake_app("wide");
    let long = "d".repeat(150);
    app.known_commands = vec![cmd("compact", "[focus]", &long)];
    typed(&mut app, "/comp");
    let mut row = |w: u16| -> String {
        let mut t = Terminal::new(TestBackend::new(w, 14)).unwrap();
        t.draw(|f| ui::draw(f, &mut app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..14)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .find(|r| r.contains("/compact"))
            .unwrap()
    };
    let wide = row(160);
    assert!(wide.matches('d').count() > 120, "the description runs far past the old 90-column limit: {}", wide.matches('d').count());
    assert!(wide.trim_end().ends_with('…') && wide.chars().count() <= 160, "and is cut with an ellipsis inside the screen");
    let narrow = row(60);
    assert!(narrow.matches('d').count() < 40 && narrow.contains('…'), "still truncates on a small screen");

    // file paths too
    app.cwd = project("wide").to_string_lossy().into();
    app.set_input("@".into());
    app.refresh_files();
    let mut t = Terminal::new(TestBackend::new(160, 14)).unwrap();
    t.draw(|f| ui::draw(f, &mut app)).unwrap();
    let buf = t.backend().buffer().clone();
    let text: String = (0..14).flat_map(|y| (0..160).map(move |x| (x, y))).map(|(x, y)| buf[(x, y)].symbol().to_string()).collect();
    assert!(text.contains("@docs/") && text.contains("@README.md"));
}

#[test]
fn model_picker_without_a_known_list_starts_a_backend_to_learn_it() {
    let (mut app, dir) = fake_app("picker2");
    run_cmd(&mut app, "/model");
    assert_eq!(wait_spawns(&dir, 1), 1);
    assert!(app.model_picker.is_some(), "it shows 'loading' until the list arrives");
}

#[test]
fn model_cache_round_trips() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-models-{}", std::process::id()));
    let path = dir.join("m").join("models.json");
    write_models(&path, &[model("sonnet", "claude-sonnet-5-5", &["low", "max"])]);
    let back = read_models(&path);
    assert_eq!((back[0].value.as_str(), back[0].resolved.as_str(), back[0].efforts.clone()), ("sonnet", "claude-sonnet-5-5", vec!["low".to_string(), "max".to_string()]));
}

fn status_app(tag: &str, interval: Option<u64>) -> (App, mpsc::Receiver<AppEvent>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-sl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(Launch::default(), dir.to_string_lossy().into(), tx);
    app.new_tab();
    let command = format!("cat > {d}/last.json; echo x >> {d}/runs; echo 'line one'; echo 'line two'", d = dir.display());
    app.statusline = Some(statusline::Config { command, interval: interval.map(Duration::from_secs), padding: 0, source: statusline::Source::Flag });
    app.statusline_on = true;
    (app, rx, dir)
}
fn runs(dir: &std::path::Path) -> usize {
    std::fs::read_to_string(dir.join("runs")).map_or(0, |t| t.lines().count())
}
fn settle(app: &mut App, rx: &mpsc::Receiver<AppEvent>) {
    let ev = rx.recv_timeout(Duration::from_secs(3)).expect("status line finished");
    app.on_event(ev);
}

#[test]
fn the_status_line_refreshes_on_change_not_on_a_timer_and_never_while_idle() {
    let (mut app, rx, dir) = status_app("policy", Some(5));
    app.tick(); // never ran: runs once
    settle(&mut app, &rx);
    assert_eq!(app.sessions[0].status_lines.len(), 2);
    assert_eq!(app.sessions[0].status_lines[0].spans[0].content, "line one");
    let sent: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("last.json")).unwrap()).unwrap();
    assert_eq!((sent["cwd"].as_str(), sent["workspace"]["current_dir"].as_str()), (Some(app.cwd.as_str()), Some(app.cwd.as_str())));
    assert_eq!(runs(&dir), 1);

    for _ in 0..5 {
        app.tick();
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((runs(&dir), rx.try_recv().is_err()), (1, true), "nothing changed, so nothing runs");

    // something changed: updates are spaced 300 ms apart (Claude Code's debounce)
    app.sessions[0].status_dirty = true;
    app.tick();
    assert_eq!(runs(&dir), 1, "too soon after the last start");
    std::thread::sleep(Duration::from_millis(350));
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 2);

    // the configured refreshInterval applies only while the user is around
    app.sessions[0].status_ran = Some(Instant::now() - Duration::from_secs(10));
    app.last_input = Instant::now() - Duration::from_secs(700);
    app.tick();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(runs(&dir), 2, "idle for ten minutes: the timer stops");
    app.last_input = Instant::now();
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 3);

    // switched off: no runs even when dirty
    app.statusline_on = false;
    app.sessions[0].status_dirty = true;
    app.sessions[0].status_ran = Some(Instant::now() - Duration::from_secs(10));
    app.tick();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(runs(&dir), 3);
}

#[test]
fn a_newer_update_cancels_the_run_in_flight_and_only_its_result_counts() {
    let (mut app, rx, dir) = status_app("cancel", None);
    let slow = format!("n=$(cat {d}/n 2>/dev/null || echo 0); echo $((n+1)) > {d}/n; if [ $n = 0 ]; then sleep 5; echo OLD; else echo NEW; fi", d = dir.display());
    app.statusline.as_mut().unwrap().command = slow;
    app.tick(); // first run: slow
    std::thread::sleep(Duration::from_millis(400));
    app.sessions[0].status_dirty = true;
    app.tick(); // second run cancels the first
    settle(&mut app, &rx); // whichever arrives first...
    if app.sessions[0].status_lines.is_empty() {
        settle(&mut app, &rx);
    }
    let shown: String = app.sessions[0].status_lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(shown, "NEW", "the cancelled run's late result is ignored");
    assert!(!app.sessions[0].status_running);
}

#[test]
fn documented_triggers_refresh_the_line_permission_mode_compact_and_reset_times() {
    let (mut app, rx, dir) = status_app("triggers", None);
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 1);
    let settle_after = |app: &mut App, ms: u64| {
        std::thread::sleep(Duration::from_millis(ms));
        app.tick();
    };
    // permission mode change (shift+tab)
    settle_after(&mut app, 350);
    app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 2, "permission mode change");
    // /compact finishing
    settle_after(&mut app, 350);
    app.sessions[0].handle(&serde_json::from_str(r#"{"type":"system","subtype":"compact_boundary"}"#).unwrap());
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 3, "/compact");
    // a limit window reaching its reset time, with no other event
    settle_after(&mut app, 350);
    app.sessions[0].status_next = Some(1.0); // long past
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 4, "reset time reached");
    settle_after(&mut app, 350);
    app.tick();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(runs(&dir), 4, "and it does not loop once the deadline is consumed");
}

#[test]
fn switching_tabs_refreshes_the_new_tab_and_each_tab_keeps_its_own_lines() {
    let (mut app, rx, dir) = status_app("tabs", None);
    app.tick();
    settle(&mut app, &rx);
    app.new_tab();
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 2);
    assert!(app.sessions[0].status_lines.len() == 2 && app.sessions[1].status_lines.len() == 2);
    std::thread::sleep(Duration::from_millis(1100));
    app.switch(-1);
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(runs(&dir), 3, "coming back to a tab refreshes it (its countdowns may be stale)");
}

#[test]
fn the_status_line_is_chosen_on_our_side_and_the_choice_is_remembered() {
    let (mut app, rx, dir) = status_app("mode", None);
    let claude_home = dir.join("claude");
    std::fs::create_dir_all(&claude_home).unwrap();
    std::fs::write(claude_home.join("settings.json"), r#"{"statusLine": {"type": "command", "command": "echo from-claude"}}"#).unwrap();
    app.claude_paths = statusline::Paths { home: Some(dir.clone()), config_dir: Some(claude_home.clone()), claude_json: None, managed: dir.join("none.json") };
    let ours = dir.join("cpp").join("config.json");
    app.config_path = Some(ours.clone());

    run_cmd(&mut app, "/statusline builtin");
    assert!(!app.statusline_on && app.statusline.is_none());
    assert_eq!(config::get_str(&ours, "statusline").as_deref(), Some("builtin"));
    app.tick();
    std::thread::sleep(Duration::from_millis(300));
    assert!(rx.try_recv().is_err() && runs(&dir) == 0, "builtin never starts a process");

    run_cmd(&mut app, "/statusline claude");
    assert!(app.statusline_on);
    let c = app.statusline.clone().unwrap();
    assert_eq!((c.command.as_str(), c.source), ("echo from-claude", statusline::Source::User));
    assert_eq!(config::get_str(&ours, "statusline").as_deref(), Some("claude"));
    app.tick();
    settle(&mut app, &rx);
    assert_eq!(app.sessions[0].status_lines[0].spans[0].content, "from-claude");

    run_cmd(&mut app, "/statusline");
    assert!(matches!(&app.sessions[0].items.last().unwrap().entry, session::Entry::Note(t) if t.contains("claude: running `echo from-claude`") && t.contains("builtin")));
    run_cmd(&mut app, "/statusline bogus");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("usage")) && app.statusline_on, "a bad word changes nothing");
    run_cmd(&mut app, "/statusline off"); // alias of builtin
    assert!(!app.statusline_on);

    // claude mode with nothing in Claude Code's settings: say so and show our own line
    std::fs::remove_file(claude_home.join("settings.json")).unwrap();
    run_cmd(&mut app, "/statusline claude");
    assert!(!app.statusline_on);
    assert!(matches!(&app.sessions[0].items.last().unwrap().entry, session::Entry::Note(t) if t.contains("no `statusLine`") && t.contains("own line")));
    assert_eq!(app.statusline_mode, statusline::Mode::Claude, "the choice stands; it applies once Claude Code has one");
}

#[test]
fn the_users_lines_sit_under_the_input_and_ours_drops_what_they_already_show() {
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _rx, _d) = status_app("draw", None);
    app.sessions[0].model = "claude-test".into();
    app.sessions[0].ctx_tokens = 5000;
    app.sessions[0].cost = 0.25;
    let rows = |app: &mut App| -> Vec<String> {
        let mut t = Terminal::new(TestBackend::new(70, 16)).unwrap();
        t.draw(|f| ui::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..16).map(|y| (0..70).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
    };
    let before = rows(&mut app);
    assert!(before[15].contains("claude-test") && before[15].contains("ctx 5.0k"), "no user lines yet: our full line");
    app.sessions[0].status_lines = vec![statusline::parse_line("\x1b[31mUSER ONE\x1b[0m"), statusline::parse_line("USER TWO")];
    let after = rows(&mut app);
    assert_eq!((after[13].as_str(), after[14].as_str()), ("USER ONE", "USER TWO"));
    assert!(!after[15].contains("claude-test") && !after[15].contains("ctx") && !after[15].contains("$0.250"), "{:?}", after[15]);
    assert!(after[15].contains("mem"), "the parts only we know remain");
    // like Claude Code, it steps aside for a permission prompt
    app.sessions[0].pending.push_back(session::Pending {
        request_id: "r".into(),
        tool: "Bash".into(),
        input: json!({"command": "ls"}),
        suggestions: json!([]),
        ui: Default::default(),
    });
    let during = rows(&mut app);
    assert!(!during.iter().any(|r| r.contains("USER ONE")), "hidden while a prompt is up");
    assert!(during[15].contains("claude-test"), "and our full line comes back with it");
    app.sessions[0].pending.clear();
    app.statusline_on = false;
    assert!(rows(&mut app)[15].contains("claude-test"), "toggled off: back to ours");
}

fn ask(app: &mut App, json_questions: &str) {
    let line = format!(r#"{{"type":"control_request","request_id":"q1","request":{{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{{"questions":{json_questions}}}}}}}"#);
    app.sessions[0].handle(&serde_json::from_str(&line).unwrap());
}
const QUESTIONS: &str = r#"[{"question":"Which?","header":"Pick","multiSelect":false,"options":[{"label":"A"},{"label":"B"}]},{"question":"Also?","header":"More","multiSelect":true,"options":[{"label":"X"},{"label":"Y"}]}]"#;

/// An app whose `claude` logs everything it is sent, so what the panel answers can be read back.
fn logging_app(tag: &str) -> (App, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let (tx, rx) = mpsc::channel();
    std::mem::forget(rx);
    let mut app = App::new(Launch { bin: script.to_string_lossy().into(), ..Default::default() }, "/".into(), tx.clone());
    app.new_tab();
    let launch = app.launch_for(0);
    app.sessions[0].warm(&launch, &tx);
    (app, dir)
}
fn control_responses(dir: &std::path::Path, want: usize) -> Vec<serde_json::Value> {
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(50));
        let got: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("stdin"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "control_response")
            .collect();
        if got.len() >= want {
            return got;
        }
    }
    panic!("no control_response arrived");
}

#[test]
fn the_question_panel_owns_the_keyboard_and_its_answers_reach_the_agent() {
    let (mut app, dir) = logging_app("qpanel");
    ask(&mut app, QUESTIONS);
    // arrows and digits drive the panel
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter); // Which? = B, moves on
    press(&mut app, KeyCode::Char('1'));
    press(&mut app, KeyCode::Char('2')); // Also? = X, Y
    press(&mut app, KeyCode::Enter); // to review
    assert_eq!(app.sessions[0].pending.len(), 1, "not sent until you submit the review");
    press(&mut app, KeyCode::Enter);
    assert!(app.sessions[0].pending.is_empty());
    let r = &control_responses(&dir, 1)[0];
    let answers = &r["response"]["response"]["updatedInput"]["answers"];
    assert_eq!((answers["Which?"].as_str(), answers["Also?"].as_str()), (Some("B"), Some("X, Y")));
}

#[test]
fn esc_declines_a_question_and_the_agent_hears_a_denial() {
    let (mut app, dir) = logging_app("qesc");
    ask(&mut app, QUESTIONS);
    typed(&mut app, "hi");
    assert!(app.input.is_empty(), "typing goes into the panel's Other field, not the message box");
    press(&mut app, KeyCode::Esc); // stops typing
    assert_eq!(app.sessions[0].pending.len(), 1);
    press(&mut app, KeyCode::Esc); // declines
    assert!(app.sessions[0].pending.is_empty());
    let r = &control_responses(&dir, 1)[0];
    assert_eq!(r["response"]["response"]["behavior"], "deny");
}

#[test]
fn app_chords_and_scrolling_still_work_while_a_panel_is_up() {
    let (mut app, _dir) = logging_app("qchords");
    ask(&mut app, QUESTIONS);
    app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert_eq!(app.sessions.len(), 2, "ctrl+t opens a tab even while a question waits in the other one");
    app.switch(-1);
    let before = app.sessions[0].scroll;
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.sessions[0].scroll, before + app.sessions[0].view_h - 2, "page keys scroll the transcript behind the panel");
    assert_eq!(app.sessions[0].pending.len(), 1, "and neither answered anything");
}

#[test]
fn pasting_fills_other_for_a_question_and_is_only_ever_a_refusal_reason_on_a_permission_prompt() {
    let (mut app, dir) = logging_app("qpaste");
    ask(&mut app, r#"[{"question":"Which?","header":"Pick","multiSelect":false,"options":[{"label":"A"},{"label":"B"}]}]"#);
    app.on_paste("2 apples".into());
    assert!(app.input.is_empty());
    press(&mut app, KeyCode::Enter);
    let answers = control_responses(&dir, 1)[0]["response"]["response"]["updatedInput"]["answers"].clone();
    assert_eq!(answers["Which?"], "2 apples", "pasted '2' did not pick option 2");

    app.sessions[0].handle(&serde_json::from_str(r#"{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[]}}"#).unwrap());
    app.on_paste("y".into());
    assert_eq!(app.sessions[0].pending.len(), 1, "a pasted y never approves");
    press(&mut app, KeyCode::Enter);
    assert!(app.sessions[0].pending.is_empty());
    let r = &control_responses(&dir, 2)[1];
    assert_eq!((r["response"]["request_id"].as_str(), r["response"]["response"]["behavior"].as_str()), (Some("p1"), Some("deny")));
    assert!(r["response"]["response"]["message"].as_str().unwrap().ends_with("instead: y"), "the paste became the reason");
}

/// Render the whole screen as text rows.
fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
    use ratatui::{Terminal, backend::TestBackend};
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect()
}

/// The consistency rule itself: whatever the surface, its panel is anchored at the bottom directly above
/// the input rule, has a titled frame, and ends in a key-hint footer.
#[test]
fn every_panel_sits_at_the_bottom_above_the_input_with_the_same_frame() {
    let (mut app, _d) = fake_app("uniform");
    app.cwd = project("uniform").to_string_lossy().into();
    app.known_commands = vec![cmd("compact", "[focus]", "summarize the conversation")];
    app.known_models = vec![model("sonnet", "claude-sonnet-5-5", &["low", "high"]), model("opus", "claude-opus-5-5", &["low", "high"])];
    app.config_path = Some(std::env::temp_dir().join(format!("claudeplusplus-uniform-cfg-{}", std::process::id())).join("config.json"));
    let mut cases: Vec<(&str, &str)> = vec![];

    // each surface is brought up the way a user would
    let mut check = |app: &mut App, name: &'static str, title: &str, hint: &str| {
        for width in [60u16, 100, 160] {
            let rows = screen(app, width, 30);
            let last = rows.len() - 1; // the status line
            if title.is_empty() {
                // The suggestions belong to the message box: they stay above it, a rule, the rows and the footer.
                assert!(rows[last - 1].starts_with("───") && rows[last - 3].starts_with("───") && rows[last - 2].starts_with("❯ "), "{name}@{width}: the message box stays\n{}", rows.join("\n"));
                assert!(rows[last - 4].contains(hint), "{name}@{width}: footer {:?}", rows[last - 4]);
                assert!(rows[..last - 4].iter().rev().take_while(|r| !r.starts_with("───")).count() >= 1 && rows.iter().all(|r| !r.contains('┌') && !r.contains('└')), "{name}@{width}: rows under a rule, no box\n{}", rows.join("\n"));
                continue;
            }
            assert!(rows[last - 1].contains(hint), "{name}@{width}: footer {:?} should contain {hint:?}\n{}", rows[last - 1], rows.join("\n"));
            assert!(rows.iter().all(|r| !r.contains("use the panel above") && !r.contains('┌') && !r.contains('└')), "{name}@{width}: no message box, no box around the panel\n{}", rows.join("\n"));
            let top = rows[..last].iter().rposition(|r| r.starts_with("────")).unwrap_or_else(|| panic!("{name}@{width}: no top rule\n{}", rows.join("\n")));
            assert_eq!(rows[top].chars().count(), width as usize, "{name}@{width}: the rule runs the full width");
            assert!(rows[top + 1].starts_with(&format!(" {title}")), "{name}@{width}: title {:?} vs {:?}", title, rows[top + 1]);
            assert!(rows[top + 2].is_empty() && rows[last - 2].is_empty(), "{name}@{width}: air under the title and above the footer");
            assert!(rows[top..last].iter().all(|r| !r.starts_with('│') && !r.ends_with('│')), "{name}@{width}: no side borders");
            assert!(rows.iter().all(|r| r.chars().count() <= width as usize), "{name}@{width}: nothing wider than the screen");
        }
        cases.push((name, "ok"));
    };

    typed(&mut app, "/comp");
    check(&mut app, "command suggestions", "", "⇥ complete");
    app.set_input("@".into());
    app.refresh_files();
    check(&mut app, "file suggestions", "", "⇥ complete");
    app.set_input(String::new());
    app.refresh_files();

    run_cmd(&mut app, "/model");
    check(&mut app, "model picker", "Model, effort & fast mode", "⎋ cancel");
    press(&mut app, KeyCode::Esc);

    run_cmd(&mut app, "/config");
    check(&mut app, "config", "Config", "⎋ close");
    press(&mut app, KeyCode::Esc);

    run_cmd(&mut app, "/statusline items");
    check(&mut app, "status line items", "Status line", "⎋ close");
    press(&mut app, KeyCode::Esc);

    app.wt_ui = Some(worktree::Ui { list: vec![worktree::Wt { path: "/a/app".into(), branch: Some("main".into()), head: "abc1234".into(), bare: false, locked: false, prunable: false }], ..Default::default() });
    check(&mut app, "worktrees", "Worktrees", "⎋ close");
    press(&mut app, KeyCode::Esc);

    app.open_ui = Some(open::Ui { sel: 0, found: vec![open::Found { target: open::Target::Url("https://example.com".into()), line: None }] });
    check(&mut app, "open", "Open", "⎋ close");
    press(&mut app, KeyCode::Esc);

    run_cmd(&mut app, "/session");
    check(&mut app, "session config", "Session", "⎋ close");
    press(&mut app, KeyCode::Esc);

    app.explorer = Some(explorer::Explorer::for_bench(vec![history::Summary { id: "abcdef12-0000".into(), title: "a session".into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 10, cwd: String::new() }]));
    let rows = screen(&mut app, 100, 30);
    assert!(rows[0].starts_with(" Sessions 1/1"), "title on the first row: {:?}", rows[0]);
    assert!(rows[29].contains("⎋ close"), "hints on the last row: {:?}", rows[29]);
    assert!(!rows.iter().any(|r| r.contains('┌') || r.contains('└') || r.starts_with("───")), "full screen has no frame, tabs or input\n{}", rows.join("\n"));
    app.explorer = None;

    ask(&mut app, QUESTIONS);
    check(&mut app, "question", "Question 1/2", "⎋ decline");
    press(&mut app, KeyCode::Esc);
    app.sessions[0].handle(&serde_json::from_str(r#"{"type":"control_request","request_id":"p1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[]}}"#).unwrap());
    check(&mut app, "permission", "Permission", "⎋ deny");
    assert_eq!(cases.len(), 10, "all ten panels were checked (the session list is full screen, checked above)");
}

#[test]
fn blocking_panels_are_yellow_and_the_ones_you_open_are_cyan() {
    use ratatui::style::Color;
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _d) = fake_app("frames");
    // The colour of the rule across the top of the panel (the first full-width rule on screen).
    let rule = |app: &mut App| -> Option<Color> {
        let mut t = Terminal::new(TestBackend::new(60, 20)).unwrap();
        t.draw(|f| ui::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..20).find(|&y| (0..60).all(|x| buf[(x, y)].symbol() == "─")).map(|y| buf[(0, y)].fg)
    };
    assert_eq!(rule(&mut app), theme::dim().fg, "no panel: only the message box's own rule");
    run_cmd(&mut app, "/config");
    assert_eq!(rule(&mut app), theme::accent().fg, "you opened it: cyan");
    press(&mut app, KeyCode::Esc);
    ask(&mut app, QUESTIONS);
    assert_eq!(rule(&mut app), theme::attention().fg, "the agent is waiting: yellow");
}

#[test]
fn config_applies_at_once_persists_and_keeps_the_keyboard() {
    let (mut app, _d) = fake_app("settings");
    let cfg = std::env::temp_dir().join(format!("claudeplusplus-settings-cfg-{}", std::process::id())).join("config.json");
    let _ = std::fs::remove_file(&cfg);
    app.config_path = Some(cfg.clone());
    run_cmd(&mut app, "/config");
    assert!(app.settings.is_some());
    typed(&mut app, "abc");
    assert!(app.input.is_empty(), "keys belong to the panel");
    app.on_paste("pasted".into());
    assert!(app.input.is_empty());

    // Tab moves to the section strip, left and right step through the sections (General, Appearance, Sessions,
    // ...), and Tab or any up/down comes back to the rows. Sessions: the profile for new tabs is its first row.
    assert!(!app.settings.as_ref().unwrap().strip);
    let before = app.default_profile.clone();
    press(&mut app, KeyCode::Tab);
    assert!(app.settings.as_ref().unwrap().strip);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.default_profile, before, "on the strip, left and right change no setting");
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.default_profile, "sources");
    assert_eq!(config::get_str(&cfg, "profile").as_deref(), Some("sources"), "saved the moment it changed");
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.default_profile, "full", "space and enter step forward, wrapping");
    press(&mut app, KeyCode::Left);
    assert_eq!(app.default_profile, "lean", "left steps back, wrapping");
    app.new_tab();
    assert_eq!(app.sessions.last().unwrap().profile, "lean", "new tabs use it");

    // Cost & memory: cache lifetime reaches every tab, then (3 rows down) idle suspend and the memory budget
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Left); // 60 -> 5 min
    assert_eq!(app.cache_ttl, Duration::from_secs(300));
    assert!(app.sessions.iter().all(|s| s.cache_ttl == Duration::from_secs(300)));
    for _ in 0..3 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(app.setting_value("idle_suspend_s"), "120", "2 min until chosen otherwise");
    press(&mut app, KeyCode::Right); // 2 min -> 5 min
    assert_eq!(app.setting_value("idle_suspend_s"), "300");
    assert_eq!(config::get_str(&cfg, "idle_suspend_s").as_deref(), Some("300"));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right); // off -> 500 MB
    assert_eq!(app.setting_value("mem_budget_mb"), "500");
    assert_eq!(config::get_str(&cfg, "mem_budget_mb").as_deref(), Some("500"));

    // Appearance, row 4: images (a process-wide mode; put it back so other tests are unaffected)
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Right); // wraps to Rate limits, the last tab
    press(&mut app, KeyCode::Right); // and on to General
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.settings.as_ref().unwrap().sel, 0, "the strip has the keys: up and down do nothing");
    press(&mut app, KeyCode::Tab); // back on the rows
    for _ in 0..4 {
        press(&mut app, KeyCode::Down);
    }
    press(&mut app, KeyCode::Right);
    assert_eq!(image::mode(), "on");
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    assert_eq!(image::mode(), "auto");

    press(&mut app, KeyCode::Esc);
    assert!(app.settings.is_none());
    assert_eq!(app.input, "", "nothing leaked into the message box");
    run_cmd(&mut app, "/setting");
    assert!(app.settings.is_some(), "/setting is /config");
}

#[test]
fn the_status_line_config_switches_modes_and_explains_when_claude_has_none() {
    let (mut app, _d) = fake_app("settings-sl");
    let dir = std::env::temp_dir().join(format!("claudeplusplus-settings-sl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    app.claude_paths = statusline::Paths { home: Some(dir.clone()), config_dir: Some(dir.join("nothing")), claude_json: None, managed: dir.join("none.json") };
    app.config_path = Some(dir.join("config.json"));
    assert_eq!(app.setting_value("statusline"), "builtin");
    run_cmd(&mut app, "/config");
    for _ in 0..4 {
        press(&mut app, KeyCode::Down); // General: the Status line row
    }
    press(&mut app, KeyCode::Right); // builtin -> claude, but Claude Code has no status line
    assert_eq!(app.setting_value("statusline"), "claude", "the choice stands");
    assert!(!app.statusline_on);
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("no `statusLine`") && f.contains("own line")), "{:?}", app.flash);
    press(&mut app, KeyCode::Right);
    assert_eq!(app.setting_value("statusline"), "builtin");
}

#[test]
fn left_arrow_at_the_start_of_the_input_opens_the_session_list_else_moves_the_cursor() {
    let (mut app, _d) = fake_app("leftkey");
    typed(&mut app, "ab");
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_none() && app.cursor == 1, "text to the left: just a cursor move");
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_none() && app.cursor == 0);
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_some(), "nothing to the left of the cursor opens the list");
    press(&mut app, KeyCode::Esc);
    assert!(app.explorer.is_none() && app.input == "ab", "closing leaves the draft alone");
}

#[test]
fn fork_opens_its_own_tab_resumed_as_a_copy_and_stop_runs_claude_stop() {
    let (mut app, dir) = fake_app("fork");
    app.open_session("aaaa1111".into(), "t".into(), None);
    app.explorer_outcome(explorer::Outcome::Fork("aaaa1111".into(), "t".into(), None));
    assert_eq!(app.sessions.len(), 2, "a copy gets its own tab even when the original is open");
    assert!(app.sessions[1].fork && !app.sessions[0].fork);
    let (launch, tx) = (app.launch_for(1), app.tx.clone());
    app.sessions[1].warm(&launch, &tx);
    app.explorer_outcome(explorer::Outcome::Stop("ab12cd34".into(), None));
    let spawns = (0..60)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            std::fs::read_to_string(dir.join("spawns")).ok().filter(|t| t.contains("--fork-session") && t.contains("stop ab12cd34"))
        })
        .expect("fork flag on the resume, and `claude stop` ran");
    assert!(spawns.contains("--resume aaaa1111"), "{spawns}");
}

#[test]
fn custom_profiles_reach_the_launch_and_the_dock_keeps_the_keyboard_after_opening() {
    let (mut app, _d) = fake_app("custprof");
    assert!(app.profile_flag_error("no-such-profile-xyz"));
    app.apply_setting("side_panel", "on");
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Enter); // opens whatever row exists, if any; must not panic on either
}

#[test]
fn scrolling_by_page_keeps_its_place_as_output_arrives_and_can_jump_back() {
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _d) = fake_app("scroll");
    for i in 0..100 {
        app.s().note(format!("line {i}"));
    }
    let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut shown = |app: &mut App| {
        t.draw(|f| ui::draw(f, app)).unwrap();
        let b = t.backend().buffer().clone();
        (0..24).map(|y| (0..80).map(|x| b[(x, y)].symbol().to_string()).collect::<String>()).collect::<Vec<_>>().join("\n")
    };
    assert!(shown(&mut app).contains("line 99") && !shown(&mut app).contains("lines below"));
    press(&mut app, KeyCode::PageUp);
    let page = app.sessions[0].view_h - 2;
    assert_eq!(app.sessions[0].scroll, page, "a page is the viewport minus two lines of overlap");
    let screen = shown(&mut app);
    assert!(screen.contains(&format!("↓ {page} lines below")) && !screen.contains("line 99"));
    let top = shown(&mut app).lines().find(|l| l.contains("line ")).unwrap().trim().to_string();
    app.s().note("new output".into());
    let after = shown(&mut app).lines().find(|l| l.contains("line ")).unwrap().trim().to_string();
    assert_eq!(top, after, "the lines in view do not move while new output arrives below");
    app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    assert_eq!(app.sessions[0].scroll, 0);
    assert!(shown(&mut app).contains("new output"));
    app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
    assert!(shown(&mut app).contains("line 0"), "ctrl+home reaches the top");
}

#[test]
fn the_dock_takes_a_left_column_only_on_wide_screens() {
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _d) = fake_app("dockdraw");
    app.apply_setting("side_panel", "on");
    app.show_dock(false);
    for (w, docked) in [(80u16, false), (120, true)] {
        let mut t = Terminal::new(TestBackend::new(w, 24)).unwrap();
        t.draw(|f| ui::draw(f, &mut app)).unwrap();
        let buf = t.backend().buffer().clone();
        let row = |y: u16| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
        assert_eq!(row(0).contains("Sessions"), docked, "{w}");
        if docked {
            assert!(row(23).trim_start().starts_with('│') || row(23).contains('│'), "dock separator spans the height");
        }
    }
}

#[test]
fn the_session_panel_sets_mode_and_tool_rules_and_takes_a_typed_rule() {
    let (mut app, _d) = fake_app("tabcfg");
    run_cmd(&mut app, "/session");
    assert!(app.tabcfg.is_some());
    typed(&mut app, "zz");
    assert!(app.input.is_empty(), "keys belong to the panel");
    assert!(screen(&mut app, 100, 30).iter().any(|r| r.starts_with(" Session")));
    app.sessions[0].mode = "default".into();
    press(&mut app, KeyCode::Right);
    assert_eq!(app.sessions[0].mode, "acceptEdits", "row 0 is the permission mode");
    press(&mut app, KeyCode::Down); // Bash
    press(&mut app, KeyCode::Right);
    assert_eq!(app.sessions[0].rule("Bash"), Some(true));
    press(&mut app, KeyCode::Right);
    assert_eq!(app.sessions[0].rule("Bash"), Some(false));
    press(&mut app, KeyCode::Char('d'));
    assert_eq!(app.sessions[0].rule("Bash"), None, "d removes the rule");
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Up); // wraps to "add a rule"
    press(&mut app, KeyCode::Enter);
    typed(&mut app, "Bash(git push:*)");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.sessions[0].rule("Bash(git push:*)"), Some(true), "typed rules start as allow");
    assert!(tabcfg::rows(&app.sessions[0]).contains(&Row::Tool("Bash(git push:*)".into(), true)));
    // the cursor is on the rule just added
    press(&mut app, KeyCode::Right);
    assert_eq!(app.sessions[0].rule("Bash(git push:*)"), Some(false));
    press(&mut app, KeyCode::Right); // past deny removes a custom rule
    assert!(app.sessions[0].rules.is_empty());
    press(&mut app, KeyCode::Esc);
    assert!(app.tabcfg.is_none());
}

#[test]
fn the_session_panel_shows_the_id_and_copies_it() {
    let (mut app, _d) = fake_app("tabcfg-id");
    run_cmd(&mut app, "/session");
    assert!(screen(&mut app, 100, 30).iter().any(|r| r.contains("Session id") && r.contains("none until the first message")));
    press(&mut app, KeyCode::Char('c'));
    assert!(app.tabcfg.is_some(), "c keeps the panel open");
    app.sessions[0].session_id = Some("abc-123".into());
    assert!(screen(&mut app, 100, 30).iter().any(|r| r.contains("Session id") && r.contains("abc-123")));
    assert_eq!(tabcfg::info(&app.sessions[0], "/x")[0].1, "abc-123");
}

#[test]
fn a_background_tab_that_finishes_or_asks_rings_once_and_the_front_tab_does_not() {
    let (mut app, _d) = fake_app("notify");
    app.new_tab();
    for i in 0..2 {
        let (launch, tx) = (app.launch_for(i), app.tx.clone());
        app.sessions[i].warm(&launch, &tx);
    }
    let (front, back) = (app.sessions[1].id, app.sessions[0].id);
    let (front_pid, back_pid) = (app.sessions[1].backend.as_ref().unwrap().pid, app.sessions[0].backend.as_ref().unwrap().pid);
    let line = |s: &str| Msg::Line(serde_json::from_str(s).unwrap());
    app.sessions[0].busy = true;
    app.sessions[1].busy = true;
    let done = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","total_cost_usd":0.1,"usage":{"input_tokens":1,"output_tokens":1}}"#;
    app.on_event(AppEvent::Backend(front, front_pid, line(done)));
    assert!(app.sent_notices.is_empty(), "the tab you are looking at stays quiet");
    app.on_event(AppEvent::Backend(back, back_pid, line(done)));
    assert_eq!(app.sent_notices, ["\x07"], "a background tab finishing rings the bell");
    app.apply_setting("notify", "desktop");
    let ask = r#"{"type":"control_request","request_id":"p","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"permission_suggestions":[]}}"#;
    app.on_event(AppEvent::Backend(back, back_pid, line(ask)));
    assert!(app.sent_notices[1].starts_with("\x1b]9;claude++: ") && app.sent_notices[1].contains("needs you"), "{:?}", app.sent_notices);
    app.apply_setting("notify", "off");
    app.sessions[0].busy = true;
    app.on_event(AppEvent::Backend(back, back_pid, line(done)));
    assert_eq!(app.sent_notices.len(), 2, "off is silent");
}

#[test]
fn copy_takes_the_last_answer_or_its_last_code_block() {
    let (mut app, _d) = fake_app("copy");
    run_cmd(&mut app, "/copy");
    assert!(app.sent_notices.is_empty(), "nothing yet, nothing copied");
    let reply = "First:\n```sh\nls\n```\nThen:\n```rust\nfn main() {}\nprintln!();\n```\ndone";
    app.s().push(Entry::Assistant { text: reply.into(), msg_id: "m".into(), streaming: false });
    run_cmd(&mut app, "/copy");
    run_cmd(&mut app, "/copy code");
    assert_eq!(app.sent_notices, [format!("copy:{reply}"), "copy:fn main() {}\nprintln!();".to_string()]);
    run_cmd(&mut app, "/copy nope");
    assert_eq!(app.sent_notices.len(), 2);
    assert_eq!(commands::last_code_block("no fences"), None);
}

#[test]
fn word_movement_and_deletion_work_on_words_and_whole_lines() {
    let (mut app, _d) = fake_app("words");
    typed(&mut app, "one two  three");
    let key = |app: &mut App, c, m| app.on_key(KeyEvent::new(c, m));
    key(&mut app, KeyCode::Left, KeyModifiers::ALT);
    assert_eq!(app.cursor, 9, "to the start of `three`");
    key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
    assert_eq!(app.cursor, 4);
    key(&mut app, KeyCode::Right, KeyModifiers::ALT);
    assert_eq!(app.cursor, 7, "to the end of `two`");
    key(&mut app, KeyCode::Char('f'), KeyModifiers::ALT);
    assert_eq!(app.cursor, 14);
    key(&mut app, KeyCode::Backspace, KeyModifiers::ALT);
    assert_eq!((app.input.as_str(), app.cursor), ("one two  ", 9));
    key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(app.input, "one ");
    app.set_input("one two".into());
    app.cursor = 4;
    key(&mut app, KeyCode::Char('d'), KeyModifiers::ALT);
    assert_eq!(app.input, "one ");
    app.set_input("keep this, drop that".into());
    app.cursor = 10;
    key(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
    assert_eq!(app.input, "keep this,");
    app.cursor = 0;
    key(&mut app, KeyCode::Left, KeyModifiers::ALT);
    assert!(app.explorer.is_none(), "alt+← at the start is not the session-list key");
}

#[test]
fn usage_asks_claude_code_shows_its_answers_and_closes() {
    let (mut app, _d) = fake_app("usage");
    run_cmd(&mut app, "/usage");
    assert!(app.usage_open);
    typed(&mut app, "zz");
    assert!(app.input.is_empty(), "keys belong to the panel");
    assert!(screen(&mut app, 100, 30).iter().any(|r| r.contains("asking Claude Code")));
    let pid = app.sessions[0].backend.as_ref().unwrap().pid;
    let sid = app.sessions[0].id;
    let reply = |v: serde_json::Value| AppEvent::Backend(sid, pid, Msg::Line(json!({"type": "control_response", "response": {"subtype": "success", "request_id": "x", "response": v}})));
    app.on_event(reply(json!({"categories": [{"name": "Skills", "tokens": 9959, "kind": "used"}, {"name": "System prompt", "tokens": 3291, "kind": "used"}], "totalTokens": 17823, "maxTokens": 1000000, "model": "claude-haiku-5-5", "skills": {"totalSkills": 78, "tokens": 9959}})));
    app.on_event(reply(json!({"rate_limits": {"five_hour": {"utilization": 41, "resets_at": "2099-01-01T00:00:00+00:00"}}})));
    let rows = screen(&mut app, 100, 30);
    let text = rows.join("\n");
    assert!(rows.iter().any(|r| r.starts_with(" Usage")) && text.contains("5-hour") && text.contains("Skills") && text.contains("17.8k of 1.0M"), "{text}");
    assert!(rows.iter().any(|r| r.contains("⎋ close")));
    press(&mut app, KeyCode::Esc);
    assert!(!app.usage_open);
}

#[test]
fn context_draws_the_window_from_the_reply_moves_refreshes_acts_and_closes() {
    let (mut app, _d) = fake_app("context");
    run_cmd(&mut app, "/context");
    assert!(app.ctx_ui.is_some() && app.overlay_open());
    assert!(screen(&mut app, 120, 40).join("\n").contains("asking Claude Code"), "until the reply arrives");
    assert!(wait_spawns(&_d, 1) >= 1);
    let (pid, sid) = (app.sessions[0].backend.as_ref().unwrap().pid, app.sessions[0].id);
    app.on_event(AppEvent::Backend(sid, pid, Msg::Line(json!({"type": "control_response", "response": {"subtype": "success", "request_id": "x", "response": context::tests::reply()}}))));
    let text = screen(&mut app, 120, 40).join("\n");
    assert!(text.contains(" Context") && text.contains("20.2k of 1.0M") && text.contains("System tools") && text.contains("last request"), "{text}");
    assert!(!text.contains("⏎ optimize"), "the first row (system tools) has no action");
    for _ in 0..3 {
        press(&mut app, KeyCode::Down);
    }
    let text = screen(&mut app, 120, 40).join("\n");
    assert!(text.contains("3 skills listed") && text.contains("⏎ optimize"), "the cursor moved to Skills: {text}");
    press(&mut app, KeyCode::Char('r'));
    assert!(app.ctx_ui.is_some() && app.sessions[0].usage.ctx.is_none(), "refresh asks again and waits for the answer");
    app.on_event(AppEvent::Backend(sid, pid, Msg::Line(json!({"type": "control_response", "response": {"subtype": "success", "request_id": "y", "response": context::tests::reply()}}))));
    press(&mut app, KeyCode::Enter);
    assert!(app.ctx_ui.is_none() && app.opt.is_some(), "enter on Skills opens /optimize");
    app.opt = None;
    run_cmd(&mut app, "/context");
    press(&mut app, KeyCode::Esc);
    assert!(app.ctx_ui.is_none() && !app.overlay_open());
}

#[test]
fn tasks_lists_the_tabs_agents_opens_one_with_its_steps_and_report_and_backs_out() {
    let (mut app, _d) = fake_app("tasks");
    run_cmd(&mut app, "/tasks");
    let rows = screen(&mut app, 100, 30);
    assert!(rows.iter().any(|r| r.contains("no subagents or background commands in this tab yet")) && rows.iter().any(|r| r.contains("close")), "{rows:?}");
    press(&mut app, KeyCode::Esc);
    assert!(app.lanes.is_none());

    let tool = |id: &str, input: serde_json::Value, result: Option<(&str, bool)>, nested| Entry::Tool { id: id.into(), name: if nested { "Bash".into() } else { "Agent".into() }, input, result: result.map(|r| (r.0.into(), r.1)), nested };
    app.s().push(tool("a", json!({"description": "count lines", "subagent_type": "Explore"}), None, false));
    app.s().push(tool("s", json!({"command": "wc -l src/*.rs"}), None, true));
    app.s().push(tool("b", json!({"description": "fix the parser"}), Some(("Fixed it in two places.", false)), false));
    run_cmd(&mut app, "/tasks");
    typed(&mut app, "zz");
    assert!(app.input.is_empty(), "keys belong to the panel");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("Tasks · 2 agents, 1 running") && text.contains("count lines") && text.contains("fix the parser") && text.contains("Explore"), "{text}");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("fix the parser") && text.contains("Fixed it in two places.") && text.contains("back"), "{text}");
    press(&mut app, KeyCode::Esc);
    assert!(app.lanes.is_some(), "esc goes back to the list first");
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Enter);
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("$ wc -l src/*.rs") && text.contains("working…"), "a running agent shows its steps live: {text}");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert!(app.lanes.is_none());
}

#[test]
fn find_jumps_to_matches_newest_first_steps_both_ways_and_wraps() {
    let (mut app, _d) = fake_app("find");
    for i in 0..60 {
        app.s().push(Entry::User(format!("question {i}{}", if i % 20 == 0 { " NEEDLE" } else { "" })));
        app.s().push(Entry::Assistant { text: format!("answer {i}"), msg_id: format!("m{i}"), streaming: false });
    }
    let top = |app: &mut App| {
        let rows = screen(app, 100, 20);
        // (the first row may be the hint naming the message above; the matches are the NEEDLE ones)
        rows.iter().find(|r| r.contains("NEEDLE")).cloned().unwrap_or_default()
    };
    assert_eq!(app.s().scroll, 0);
    run_cmd(&mut app, "/find needle");
    let first = top(&mut app);
    assert!(first.contains("question 40"), "newest match first, brought to the top: {first}");
    run_cmd(&mut app, "/find");
    assert!(top(&mut app).contains("question 20"));
    run_cmd(&mut app, "/find");
    run_cmd(&mut app, "/find");
    assert!(top(&mut app).contains("question 40"), "wraps around to the newest");
    run_cmd(&mut app, "/find +");
    assert!(top(&mut app).contains("question 0 NEEDLE"), "newer from 40 wraps to the oldest: {}", top(&mut app));
    run_cmd(&mut app, "/find zzzz-nothing");
    assert_eq!(app.flash.as_deref().map(|f| f.contains("no match")), Some(true));
}

#[test]
fn ctrl_z_undoes_typing_in_bursts_and_other_edits_one_by_one_and_forgets_on_send() {
    let (mut app, _d) = fake_app("undo");
    let key = |app: &mut App, c, m| app.on_key(KeyEvent::new(c, m));
    typed(&mut app, "hello world");
    key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL); // delete a word
    assert_eq!(app.input, "hello ");
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!((app.input.as_str(), app.cursor), ("hello world", 11), "the deletion is one step");
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(app.input, "", "the whole burst of typing is one step");
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("nothing to undo")));
    typed(&mut app, "draft");
    app.new_tab();
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(app.input, "", "another tab's box has no history of the first");
    app.switch(-1);
    assert_eq!(app.input, "draft");
}

#[test]
fn a_project_can_pick_its_own_profile_unless_one_was_given_on_the_command_line() {
    let (mut app, _d) = fake_app("projprof");
    let proj = project("projprof");
    std::fs::create_dir_all(proj.join(".claude++")).unwrap();
    std::fs::write(proj.join(".claude++/config.json"), r#"{"profile":"lean"}"#).unwrap();
    app.cwd = proj.to_string_lossy().into();
    app.new_tab();
    assert_eq!(app.sessions.last().unwrap().profile, "lean", "tabs started in the project use its profile");
    app.s().note("busy tab".into());
    app.open_session("aaaa0000-0000".into(), String::new(), Some("/nonexistent-elsewhere".into()));
    assert_eq!(app.sessions[app.active].profile, "full", "another directory has no say: the default applies");
    std::fs::write(proj.join(".claude++/config.json"), r#"{"profile":"no-such-profile"}"#).unwrap();
    app.new_tab();
    assert_eq!(app.sessions.last().unwrap().profile, "full", "an unknown name is ignored");
    std::fs::write(proj.join(".claude++/config.json"), r#"{"profile":"lean"}"#).unwrap();
    app.profile_forced = true;
    app.new_tab();
    assert_eq!(app.sessions.last().unwrap().profile, "full", "--profile wins");
}

#[test]
fn closing_a_busy_tab_needs_a_second_press_but_an_idle_one_closes_at_once() {
    let (mut app, _d) = fake_app("closebusy");
    app.new_tab();
    app.sessions[1].busy = true;
    run_cmd(&mut app, "/close");
    assert_eq!(app.sessions.len(), 2);
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("still working")));
    run_cmd(&mut app, "/close");
    assert_eq!(app.sessions.len(), 1, "the second press closes it");
    app.new_tab();
    run_cmd(&mut app, "/close");
    assert_eq!(app.sessions.len(), 1, "an idle tab needs no confirmation");
}

#[test]
fn each_tab_keeps_its_own_draft() {
    let (mut app, _d) = fake_app("drafts");
    typed(&mut app, "first draft");
    app.new_tab();
    assert!(app.input.is_empty(), "a new tab starts with an empty box");
    typed(&mut app, "second");
    app.switch(-1);
    assert_eq!((app.input.as_str(), app.cursor), ("first draft", 11));
    app.switch(1);
    assert_eq!(app.input, "second");
    app.close_tab();
    assert_eq!(app.input, "first draft", "closing a tab brings back the neighbour's draft");
    app.switch(1);
    assert_eq!(app.input, "first draft");
}

#[test]
fn right_arrow_closes_the_dock_and_steps_back_out_of_the_full_screen_list() {
    let (mut app, _d) = fake_app("rightkey");
    app.apply_setting("side_panel", "on");
    // Empty lists: on a session, → opens it (see below); with nothing highlighted it steps out.
    let empty = || Some(explorer::Explorer::for_bench(vec![]));
    press(&mut app, KeyCode::Left);
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_some());
    app.explorer = empty();
    press(&mut app, KeyCode::Right);
    assert!(app.explorer.is_none() && app.dock.is_some() && app.dock_focus, "→ leaves full screen for the dock");
    app.dock = empty();
    press(&mut app, KeyCode::Right);
    assert!(app.dock.is_none(), "→ closes the dock");
    app.apply_setting("side_panel", "off");
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_some() && app.dock.is_none(), "full screen is always available");
    app.explorer = empty();
    press(&mut app, KeyCode::Right);
    assert!(app.explorer.is_none() && app.dock.is_none(), "→ closes it");
}

#[test]
fn in_panel_mode_left_docks_the_list_then_goes_full_screen_and_esc_steps_back() {
    let (mut app, _d) = fake_app("dock");
    app.apply_setting("side_panel", "on");
    press(&mut app, KeyCode::Left);
    assert!(app.dock.is_some() && app.dock_focus && app.explorer.is_none(), "first ← docks and focuses");
    typed(&mut app, "hi");
    assert!(app.dock_focus && app.input.is_empty(), "typing is ignored while the list has the keys");
    app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.dock_focus, "ctrl+c does not take the keys from the panel");
    press(&mut app, KeyCode::Esc);
    assert!(app.dock.is_none() && !app.dock_focus, "esc hides the panel: there is no docked-but-idle state");
    typed(&mut app, "hi");
    assert_eq!(app.input, "hi");
    // ctrl+x in the focused panel stops the agent the cursor is on (here: the working tab itself).
    app.sessions[0].session_id = Some("aaa111".into());
    app.sessions[0].busy = true;
    app.dock = Some(explorer::Explorer::for_bench(vec![history::Summary { id: "aaa111".into(), title: "T".into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() }]));
    let open = app.open_states();
    app.dock.as_mut().unwrap().set_open(open);
    app.dock_focus = true;
    press(&mut app, KeyCode::Down); // off the group heading, onto the session
    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(app.dock_focus, "the panel keeps the keys");
    assert!(app.flash.as_deref() == Some("stopped"), "{:?}", app.flash);
    app.sessions[0].busy = false;
    app.dock_focus = false;
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Left);
    assert!(app.dock_focus, "← at the start refocuses it");
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_some(), "second ← goes full screen");
    press(&mut app, KeyCode::Esc);
    assert!(app.explorer.is_none() && app.dock_focus, "Esc steps back to the focused dock");
    press(&mut app, KeyCode::Esc);
    assert!(app.dock.is_none() && !app.dock_focus, "Esc hides it");
    app.toggle_sessions();
    assert!(app.dock.is_some() && app.dock_focus, "/sessions brings it back, focused");
    app.toggle_sessions();
    assert!(app.dock.is_none(), "and hides it");
    app.apply_setting("side_panel", "off");
    press(&mut app, KeyCode::Home);
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_some() && app.dock.is_none(), "full mode goes straight to the full screen");
}

#[test]
fn the_explorer_takes_keys_and_reopens_into_a_blank_tab_else_a_new_one() {
    let (mut app, _d) = fake_app("explore");
    app.explorer = Some(explorer::Explorer::new("/nonexistent-dir", vec![]));
    typed(&mut app, "abc");
    assert!(app.input.is_empty(), "keys go to the explorer, not the message box");
    press(&mut app, KeyCode::Esc);
    assert!(app.explorer.is_none());

    app.open_session("11111111-aaaa".into(), "First chat title that is long".into(), None);
    assert_eq!(app.sessions.len(), 1, "blank tab is reused");
    assert_eq!(app.sessions[0].title, "First chat title t");
    app.open_session("22222222-bbbb".into(), "Second".into(), None);
    assert_eq!(app.sessions.len(), 2, "a used tab is kept");
    assert_eq!(app.active, 1);
}

/// Prints redraw cost with a 3000-entry transcript (run with `cargo test --release -- --nocapture`).
/// Frame cost of every surface at a large terminal size. Run with
/// `cargo test --release -- --ignored --nocapture frame_budget` and compare to DESIGN.md's budget.
#[test]
#[ignore]
fn frame_budget() {
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _d) = fake_app("budget");
    for i in 0..3000 {
        app.sessions[0].push(session::Entry::User(format!("question {i}")));
        app.sessions[0].push(session::Entry::Assistant {
            text: format!("# Heading {i}\n\nSome **bold** text with `code` and a [link](https://x.dev), then a list:\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |"),
            msg_id: String::new(),
            streaming: false,
        });
    }
    app.known_commands = (0..60).map(|i| cmd(&format!("command-{i}"), "[arg]", &"describes what it does ".repeat(6))).collect();
    app.known_models = (0..13).map(|i| model(&format!("m{i}"), &format!("id{i}"), &["low", "medium", "high"])).collect();
    let mut term = Terminal::new(TestBackend::new(200, 50)).unwrap();
    let mut measure = |label: &str, app: &mut App| {
        term.draw(|f| ui::draw(f, app)).unwrap(); // warm the wrap caches
        let n = 300;
        let t = Instant::now();
        for _ in 0..n {
            term.draw(|f| ui::draw(f, app)).unwrap();
        }
        println!("{label:<34} {:>7.0} µs/frame", t.elapsed().as_secs_f64() * 1e6 / n as f64);
    };
    measure("transcript only (3000 turns)", &mut app);
    app.set_input("/c".into());
    measure("+ command popup (60 commands)", &mut app);
    app.set_input("a long message ".repeat(30));
    measure("+ 7-line input", &mut app);
    app.set_input(String::new());
    app.model_picker = Some(ModelPicker { sel: 3, effort: 1, fast: false, focus: 0 });
    measure("+ model picker", &mut app);
    app.model_picker = None;
    let sums: Vec<_> = (0..200)
        .map(|i| history::Summary { id: format!("{i:08}-aaaa"), title: format!("session {i}"), prompt: "first prompt".into(), modified: std::time::SystemTime::now(), size: 4096, cwd: String::new() })
        .collect();
    app.explorer = Some(explorer::Explorer::for_bench(sums));
    measure("+ session explorer (200)", &mut app);
    app.explorer = None;
    app.sessions[0].pending.push_back(session::Pending {
        request_id: "q".into(),
        tool: "AskUserQuestion".into(),
        input: json!({"questions": [{"question": "Which approach?", "header": "Approach", "multiSelect": false, "options": [
            {"label": "A", "description": "first way"}, {"label": "B", "description": "second way"}, {"label": "C", "description": "third way"}]}]}),
        suggestions: json!([]),
        ui: Default::default(),
    });
    measure("+ question panel", &mut app);
}

#[test]
fn redraw_cost_stays_flat_on_a_long_transcript() {
    use ratatui::{Terminal, backend::TestBackend};
    let (tx, _rx) = mpsc::channel();
    std::mem::forget(_rx);
    let mut app = App::new(Launch::default(), "/".into(), tx);
    app.new_tab();
    let text = "lorem ipsum dolor sit amet ".repeat(20);
    for i in 0..1500 {
        app.sessions[0].push(session::Entry::User(format!("question {i}")));
        app.sessions[0].push(session::Entry::Assistant { text: text.clone(), msg_id: String::new(), streaming: false });
    }
    app.sessions[0].push(session::Entry::Assistant { text: String::new(), msg_id: "m".into(), streaming: true });
    let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let t = Instant::now();
    term.draw(|f| ui::draw(f, &mut app)).unwrap();
    let first = t.elapsed();
    let n = 200;
    let t = Instant::now();
    for _ in 0..n {
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
    }
    let steady = t.elapsed() / n;
    // Streaming: each delta invalidates and re-wraps only the last entry.
    let t = Instant::now();
    for i in 0..n {
        let item = app.sessions[0].items.last_mut().unwrap();
        if let session::Entry::Assistant { text, .. } = &mut item.entry {
            text.push_str("token ");
        }
        item.cache = None;
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        let _ = i;
    }
    let streaming = t.elapsed() / n;
    eprintln!("3000 entries: first draw {first:?}, steady redraw {steady:?}, streaming redraw {streaming:?}");
    assert!(steady < Duration::from_millis(50) && streaming < Duration::from_millis(50));
}

#[test]
fn permissions_panel_adds_and_deletes_in_the_chosen_file_and_confirms_a_delete() {
    let (mut app, d) = fake_app("perms");
    let proj = d.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    app.cwd = proj.to_string_lossy().into();
    let local = proj.join(".claude/settings.local.json");
    permissions::add(&local, permissions::Behavior::Ask, "Bash(rm:*)").unwrap();
    run_cmd(&mut app, "/permissions");
    assert!(app.perms.is_some());
    typed(&mut app, "abc");
    assert!(app.input.is_empty(), "keys belong to the panel");

    press(&mut app, KeyCode::Right);
    assert_eq!(app.perms.as_ref().unwrap().tab, permissions::Behavior::Allow, "the rules have the keys: left and right do nothing");
    press(&mut app, KeyCode::Tab); // the allow/ask/deny strip has them now
    press(&mut app, KeyCode::Down);
    assert_eq!(app.perms.as_ref().unwrap().sel, 0, "and up and down do nothing");
    press(&mut app, KeyCode::Right); // Ask tab
    assert_eq!(app.perms.as_ref().unwrap().tab, permissions::Behavior::Ask);
    press(&mut app, KeyCode::Tab); // back to the rules
    press(&mut app, KeyCode::Down);
    assert_eq!(app.perms.as_ref().unwrap().at().map(|r| r.text.as_str()), Some("Bash(rm:*)"));
    press(&mut app, KeyCode::Char('d'));
    assert!(std::fs::read_to_string(&local).unwrap().contains("Bash(rm:*)"), "one press only asks");
    press(&mut app, KeyCode::Char('d'));
    assert!(!std::fs::read_to_string(&local).unwrap().contains("Bash(rm:*)"), "the second press deletes");

    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Left); // Allow tab
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Enter);
    typed(&mut app, "Read(./x)");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter); // first location: local
    assert!(std::fs::read_to_string(&local).unwrap().contains("\"Read(./x)\""));
    assert_eq!(app.perms.as_ref().unwrap().at().map(|r| r.text.as_str()), Some("Read(./x)"), "the cursor lands on the new rule");
    press(&mut app, KeyCode::Esc);
    assert!(app.perms.is_none());
}

#[test]
fn folded_groups_are_shared_between_the_full_list_and_the_side_panel() {
    let (mut app, _d) = fake_app("folds");
    let sum = |id: &str| history::Summary { id: id.into(), title: "t".into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 10, cwd: String::new() };
    app.explorer = Some(explorer::Explorer::for_bench(vec![sum("abcdef12-0000")]));
    press(&mut app, KeyCode::Up); // onto the "earlier" heading
    press(&mut app, KeyCode::Enter); // fold it
    press(&mut app, KeyCode::Esc);
    assert!(app.explorer.is_none());
    assert_eq!(app.list_prefs.1, vec!["○ earlier".to_string()]);
    app.show_dock(false);
    assert_eq!(app.dock.as_ref().unwrap().folds(), ["○ earlier".to_string()], "the side panel opens with the same folds");
}

#[test]
fn a_conversations_colour_tints_its_tab() {
    let (mut app, _d) = fake_app("tint");
    app.sessions[0].title = "first".into();
    app.new_tab();
    app.sessions[1].title = "second".into();
    run_cmd(&mut app, "/color purple");
    let tint = theme::tint(Some("purple")).fg;
    let line = ui::tab_line(&app);
    let fg_of = |text: &str| line.spans.iter().find(|s| s.content.contains(text)).and_then(|s| s.style.fg);
    assert_eq!(app.sessions[1].color.as_deref(), Some("purple"));
    assert_eq!(fg_of("2"), tint, "the tab in front wears the colour");
    assert_eq!(fg_of("first"), theme::dim().fg, "an uncoloured tab keeps the plain look");
    app.goto(0);
    assert_eq!(fg_of("2"), tint, "and so does the number of one behind");
    app.goto(1);
    run_cmd(&mut app, "/color nonsense");
    run_cmd(&mut app, "/color default");
    assert_eq!(app.sessions[1].color, None, "default clears it");
}

#[test]
fn the_colour_picker_previews_the_ui_and_applies_through_the_command() {
    let (mut app, _d) = fake_app("colourpick");
    run_cmd(&mut app, "/color");
    assert_eq!(app.color_picker, Some(0), "opens on `default` when nothing is set");
    assert!(app.overlay_open());
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // blue
    assert_eq!(App::color_choices()[app.color_picker.unwrap()], "blue", "the UI previews the highlighted colour");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.color_picker.is_none());
    assert_eq!(app.sessions[0].color, None, "Esc leaves the colour alone");
    run_cmd(&mut app, "/color");
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.sessions[0].color.as_deref(), Some("red"));
    run_cmd(&mut app, "/color");
    assert_eq!(app.color_picker, Some(1), "reopens on the current colour");
    // The tab in front and the whole UI wear the highlighted colour, others keep theirs.
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // blue
    assert_eq!(app.accent_color().as_deref(), Some("blue"));
    assert_eq!(app.tab_color(0), Some("blue"));
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)); // default
    assert_eq!(app.accent_color(), None, "`default` previews as no colour");
    assert_eq!(app.tab_color(0), None);
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.tab_color(0), Some("red"));
}

#[test]
fn browsing_conversations_wears_the_highlighted_ones_colour() {
    let (mut app, _d) = fake_app("browsecolour");
    app.new_tab();
    app.sessions[1].session_id = Some("aaa111".into());
    app.sessions[1].color = Some("green".into());
    app.sessions[0].color = Some("red".into());
    app.active = 0;
    assert_eq!(app.accent_color().as_deref(), Some("red"));
    let sum = |id: &str| history::Summary { id: id.into(), title: "t".into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() };
    app.explorer = Some(explorer::Explorer::for_bench(vec![sum("aaa111")]));
    assert_eq!(app.accent_color().as_deref(), Some("green"), "the full-screen list wears the highlighted tab's colour");
    app.explorer = Some(explorer::Explorer::for_bench(vec![sum("zzz999")]));
    assert_eq!(app.accent_color(), None, "a stored conversation without a colour is plain");
    app.explorer = None;
    app.dock = Some(explorer::Explorer::for_bench(vec![sum("aaa111")]));
    assert_eq!(app.accent_color().as_deref(), Some("red"), "an unfocused side panel changes nothing");
    app.dock_focus = true;
    assert_eq!(app.accent_color().as_deref(), Some("green"));
}

#[test]
fn every_list_rests_on_the_same_conversation() {
    let sm = |id: &str| history::Summary { id: id.into(), title: id.into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() };
    let (mut app, _d) = fake_app("selection");
    app.apply_setting("side_panel", "on");
    app.sessions[0].session_id = Some("aaa111".into());
    app.dock = Some(explorer::Explorer::for_bench(vec![sm("aaa111"), sm("bbb222")]));
    app.dock.as_mut().unwrap().select("bbb222");
    app.sync_selection();
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("aaa111"), "an unfocused panel rests on the conversation in view");
    // The full screen list leads while it is open, and Esc hands its row to the panel.
    app.explorer = Some(explorer::Explorer::for_bench(vec![sm("aaa111"), sm("bbb222")]));
    app.explorer.as_mut().unwrap().select("bbb222");
    press(&mut app, KeyCode::PageUp); // any key that leaves the row alone
    app.explorer.as_mut().unwrap().select("bbb222");
    app.sync_selection();
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("bbb222"));
    press(&mut app, KeyCode::Esc);
    assert!(app.explorer.is_none() && app.dock_focus);
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("bbb222"), "the panel keeps the row Esc came from");
}

#[test]
fn the_side_panel_takes_the_keys_on_the_conversation_in_view_and_filters_after_a_slash() {
    let sm = |id: &str, t: &str| history::Summary { id: id.into(), title: t.into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() };
    let (mut app, _d) = fake_app("dockfilter");
    app.apply_setting("side_panel", "on");
    app.sessions[0].session_id = Some("bbb222".into());
    app.dock = Some(explorer::Explorer::for_bench(vec![sm("aaa111", "alpha"), sm("bbb222", "beta"), sm("ccc333", "gamma")]));
    app.dock.as_mut().unwrap().select("aaa111"); // a stale cursor, left from before
    press(&mut app, KeyCode::Left);
    assert!(app.dock_focus);
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("bbb222"), "focusing starts on the conversation in view");
    typed(&mut app, "gam");
    assert!(!app.dock.as_ref().unwrap().filtering() && app.input.is_empty(), "letters alone do nothing");
    press(&mut app, KeyCode::Char('/'));
    typed(&mut app, "gam");
    assert!(app.dock.as_ref().unwrap().filtering() && app.dock_focus && app.input.is_empty());
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("ccc333"));
    press(&mut app, KeyCode::Esc);
    assert!(app.dock_focus && !app.dock.as_ref().unwrap().filtering(), "the first esc ends the filter");
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("ccc333"), "and keeps the cursor where it was");
    press(&mut app, KeyCode::Esc);
    assert!(!app.dock_focus && app.dock.is_none());
}

#[test]
fn a_conversation_just_opened_from_a_list_is_the_selected_one_before_its_backend_reports_an_id() {
    let sm = |id: &str| history::Summary { id: id.into(), title: id.into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() };
    let (mut app, _d) = fake_app("justopened");
    app.apply_setting("side_panel", "on");
    app.explorer_outcome(explorer::Outcome::Open("bbb222".into(), "B".into(), None));
    assert!(app.sessions[app.active].session_id.is_none(), "no id until the first turn");
    assert_eq!(app.open_states().first().map(|s| s.0.clone()).as_deref(), Some("bbb222"), "yet the tab counts as open on it");
    app.dock = Some(explorer::Explorer::for_bench(vec![sm("aaa111"), sm("bbb222"), sm("ccc333")]));
    press(&mut app, KeyCode::Left);
    assert!(app.dock_focus);
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("bbb222"));
}

#[test]
fn a_list_opens_at_once_before_it_is_read_and_fills_in_on_the_cursors_conversation() {
    let sm = |id: &str| history::Summary { id: id.into(), title: id.into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() };
    let (mut app, _d) = fake_app("lazylist");
    app.sessions[0].session_id = Some("bbb222".into());
    app.apply_setting("side_panel", "on");
    press(&mut app, KeyCode::Left); // nothing read yet
    assert!(app.dock.is_some() && app.dock_focus, "the panel is up without waiting");
    assert!(screen(&mut app, 140, 30).join("\n").contains("loading…"));
    app.on_event(AppEvent::Sessions((vec![sm("aaa111"), sm("bbb222"), sm("ccc333")], vec![])));
    assert!(app.list_cache.is_some());
    assert_eq!(app.dock.as_ref().unwrap().selected().map(|s| s.0).as_deref(), Some("bbb222"), "the rows arrive with the cursor on the conversation in view");
    let text = screen(&mut app, 140, 30).join("\n");
    assert!(!text.contains("loading…"));
    // The next list opens straight from what was read.
    app.hide_dock();
    press(&mut app, KeyCode::Left);
    assert!(screen(&mut app, 140, 30).join("\n").contains("bbb222"));
    // What was sent before arrives later and goes under what was sent since.
    app.history = vec!["new".into()];
    app.on_event(AppEvent::Sent(vec!["old".into()]));
    assert_eq!(app.history, vec!["old".to_string(), "new".to_string()]);
}

#[test]
fn a_resumed_conversation_arrives_later_and_nothing_is_sent_until_then() {
    let (mut app, _d) = fake_app("asyncresume");
    let mine = app.sessions[0].id;
    app.sessions[0].loading = true; // what a real resume sets while its thread reads the file
    app.input = "hello".into();
    app.cursor = 5;
    app.submit();
    assert_eq!(app.input, "hello", "kept, not sent");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("still reading")));
    // A read for another tab, or one from before a /clear, changes nothing.
    app.sessions[0].clear();
    app.sessions[0].loading = true;
    app.on_event(AppEvent::Resumed(mine + 99, 0, "x".into(), Box::new(history::Loaded::read(std::path::Path::new("/nonexistent")))));
    assert!(app.sessions[0].loading, "not this tab's");
    app.on_event(AppEvent::Resumed(mine, 0, "x".into(), Box::new(history::Loaded::read(std::path::Path::new("/nonexistent")))));
    assert!(app.sessions[0].loading, "stale: the tab was cleared after that read began");
}

fn mouse_ev(app: &mut App, kind: ratatui::crossterm::event::MouseEventKind, x: u16, y: u16) {
    use ratatui::crossterm::event::MouseEvent;
    app.on_event(AppEvent::Term(Event::Mouse(MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE })));
}

#[test]
fn the_wheel_scrolls_the_conversation_and_never_touches_the_input_history() {
    use ratatui::crossterm::event::MouseEventKind::{ScrollDown, ScrollUp};
    let (mut app, _d) = fake_app("wheel");
    for i in 0..100 {
        app.s().note(format!("line {i}"));
    }
    app.history = vec!["an old message".into()];
    screen(&mut app, 80, 24);
    mouse_ev(&mut app, ScrollUp, 5, 5);
    assert_eq!(app.sessions[0].scroll, 3);
    assert!(screen(&mut app, 80, 24).join("\n").contains("↓ 3 lines below"));
    mouse_ev(&mut app, ScrollDown, 5, 5);
    assert_eq!(app.sessions[0].scroll, 0);
    assert!(app.input.is_empty(), "the history was not walked");
}

#[test]
fn dragging_selects_and_copies_on_release_and_double_and_triple_clicks_take_a_word_and_a_line() {
    use ratatui::crossterm::event::{MouseButton::Left, MouseEventKind::{Down, Drag, Up}};
    let (mut app, _d) = fake_app("select");
    app.s().push(session::Entry::Assistant { text: "alpha beta gamma\n\nsecond paragraph here".into(), msg_id: String::new(), streaming: false });
    let rows = screen(&mut app, 100, 24); // the app learns its panes from this frame
    let (y, x) = rows.iter().enumerate().find_map(|(y, r)| r.find("alpha").map(|i| (y as u16, r[..i].chars().count() as u16))).unwrap();
    let press = |app: &mut App, x: u16, y: u16, to: u16| {
        mouse_ev(app, Down(Left), x, y);
        screen(app, 100, 24); // the frame drawn while the button is down takes the screen
        mouse_ev(app, Drag(Left), to, y);
        screen(app, 100, 24); // each frame reads the text under the ends, and the copy follows it
        mouse_ev(app, Up(Left), to, y);
        screen(app, 100, 24);
    };
    press(&mut app, x, y, x + 9);
    assert!(app.sent_notices.contains(&"copy:alpha beta".to_string()), "{:?}", app.sent_notices);
    assert!(screen(&mut app, 100, 24).len() == 24 && app.sessions[0].sel.span().is_some(), "the selection stays drawn");
    app.on_event(AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))));
    assert!(app.sessions[0].sel.span().is_none(), "a key drops it");
    // Double click: the word under the pointer.
    app.sent_notices.clear();
    press(&mut app, x + 7, y, x + 7);
    press(&mut app, x + 7, y, x + 7);
    assert!(app.sent_notices.contains(&"copy:beta".to_string()), "{:?}", app.sent_notices);
    // Triple click: the whole line.
    press(&mut app, x + 7, y, x + 7);
    assert!(app.sent_notices.iter().any(|n| n.ends_with("alpha beta gamma")), "{:?}", app.sent_notices);
}

#[test]
fn redo_undoes_an_undo_and_any_new_edit_forgets_it() {
    let (mut app, _d) = fake_app("redo");
    let key = |app: &mut App, c, m| app.on_key(KeyEvent::new(c, m));
    typed(&mut app, "abc");
    key(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(app.input, "abc");
    key(&mut app, KeyCode::Char('Z'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
    assert_eq!((app.input.as_str(), app.cursor), ("", 0), "ctrl+shift+z redoes the deletion");
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    key(&mut app, KeyCode::Char('z'), KeyModifiers::ALT);
    assert_eq!(app.input, "", "alt+z redoes too");
    key(&mut app, KeyCode::Char('z'), KeyModifiers::ALT);
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("nothing to redo")));
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    typed(&mut app, "x");
    key(&mut app, KeyCode::Char('z'), KeyModifiers::ALT);
    assert_eq!(app.input, "abcx", "typing after an undo drops what could be redone");
}

#[test]
fn up_and_down_move_between_rows_before_they_walk_history_and_the_draft_comes_back() {
    let (mut app, _d) = fake_app("rows");
    app.input_w = 10;
    app.history = vec!["old one".into(), "old two".into()];
    typed(&mut app, "first line");
    app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    typed(&mut app, "second");
    assert_eq!(app.cursor, 17);
    press(&mut app, KeyCode::Up);
    assert_eq!((app.input.as_str(), app.cursor), ("first line\nsecond", 6), "↑ goes to the row above, same column");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.cursor, 17, "↓ goes back down");
    press(&mut app, KeyCode::Down);
    assert_eq!(app.input, "first line\nsecond", "↓ on the last row with no history open leaves the text alone");
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "old two", "from the first row ↑ is history");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.input, "old one");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.input, "first line\nsecond", "walking back past the newest brings the draft back");
    // Home / End / ctrl+k / ctrl+u work on the line, not the whole message.
    press(&mut app, KeyCode::Home);
    assert_eq!(app.cursor, 11);
    app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(app.input, "first line\n");
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.input, "first line", "at the start of a line, ctrl+u takes the break");
    press(&mut app, KeyCode::Home);
    app.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(app.input, "");
}

#[test]
fn right_arrow_opens_the_highlighted_conversation() {
    // → on a session in the list opens it, in the full screen list and the dock.
    let mut e = explorer::Explorer::for_bench(vec![history::Summary { id: "aaa111".into(), title: "T".into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() }]);
    assert!(matches!(e.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)), explorer::Outcome::OpenHere(id, ..) if id == "aaa111"));

    // ...replacing the tab in front, not adding one; a tab mid-turn is left alone.
    let (mut app, _d) = fake_app("openhere");
    app.s().note("an old conversation".into());
    app.sessions[0].busy = true;
    app.explorer_outcome(explorer::Outcome::OpenHere("aaa111".into(), "T".into(), None));
    assert_eq!((app.sessions.len(), app.sessions[0].title.as_str()), (2, ""), "a working tab is not thrown away: new tab instead");
    app.sessions.pop();
    app.active = 0;
    app.sessions[0].busy = false;
    app.explorer_outcome(explorer::Outcome::OpenHere("aaa111".into(), "T".into(), None));
    assert_eq!(app.sessions.len(), 1, "same tab");
    assert_eq!(app.sessions[0].title, "T");

    // From the side panel, → also hides the panel; Enter leaves it docked.
    app.apply_setting("side_panel", "on");
    app.show_dock(true);
    app.dock = Some(explorer::Explorer::for_bench(vec![history::Summary { id: "bbb222".into(), title: "B".into(), prompt: "p".into(), modified: std::time::SystemTime::now(), size: 1, cwd: String::new() }]));
    press(&mut app, KeyCode::Right);
    assert!(app.dock.is_none() && app.sessions.len() == 1, "→ opens here and hides the side panel");
}

#[test]
fn a_command_run_folds_per_call_by_alt_o_and_failures_show_their_end() {
    let (mut app, _d) = fake_app("fold");
    let out: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    let bash = |id: &str, cmd: &str, r: &str, err| Entry::Tool { id: id.into(), name: "Bash".into(), input: json!({"command": cmd, "description": "why"}), result: Some((r.into(), err)), nested: false };
    app.s().push(bash("a", "seq 20", &out, false));
    app.s().push(bash("b", "make\nmake test", &out, true));
    let text = screen(&mut app, 80, 40).join("\n");
    assert!(text.contains("line 4") && !text.contains("line 5") && text.contains("+16 lines"), "a passing run shows its start: {text}");
    assert!(text.contains("16 earlier lines") && text.contains("line 20") && !text.contains("line 16"), "a failing one its end: {text}");
    assert!(text.contains("whole command"), "{text}");
    // alt+o flips the latest call; ctrl+o resets them all to the global setting.
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT));
    let text = screen(&mut app, 80, 80).join("\n");
    assert!(!text.contains("earlier lines"), "{text}");
    assert!(text.contains("make test") && text.contains("why"), "unfolded, the whole command and what it is for: {text}");
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(app.s().items.iter().all(|i| !i.flipped));
}

fn alt(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::ALT));
}

fn queued(app: &mut App) -> Vec<String> {
    app.s().queue.iter().map(|q| q.text.clone()).collect()
}

fn sent(app: &mut App) -> Vec<String> {
    app.s().items.iter().filter_map(|i| if let Entry::User(t) = &i.entry { Some(t.clone()) } else { None }).collect()
}

#[test]
fn messages_sent_while_claude_works_wait_in_a_queue_you_can_reorder_edit_and_send() {
    let (mut app, _d) = fake_app("queue");
    app.s().busy = true;
    for m in ["one", "two", "three"] {
        typed(&mut app, m);
        press(&mut app, KeyCode::Enter);
    }
    assert_eq!(queued(&mut app), ["one", "two", "three"]);
    assert!(sent(&mut app).is_empty(), "nothing went to Claude");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains(&theme::keys("alt+q to manage")) && text.contains("two"), "{text}");

    alt(&mut app, KeyCode::Char('q'));
    alt(&mut app, KeyCode::Down);
    assert_eq!(queued(&mut app), ["two", "one", "three"], "alt+↓ moves the message under the cursor down");
    press(&mut app, KeyCode::Char('t'));
    assert_eq!(queued(&mut app), ["one", "two", "three"], "t brings it back to the top");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('d'));
    assert_eq!(queued(&mut app), ["one", "three"]);
    typed(&mut app, "z");
    assert!(app.input.is_empty(), "the panel owns the keys");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("Queue · 2 waiting") && text.contains("send now"), "{text}");
    press(&mut app, KeyCode::Char('e'));
    assert!(app.input == "three" && queued(&mut app) == ["one"] && app.queue_ui.is_none(), "e takes it back into the box");

    // With text in the box, editing another would lose it: refused, panel stays.
    alt(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('e'));
    assert!(app.queue_ui.is_some() && queued(&mut app) == ["one"] && app.input == "three");
    press(&mut app, KeyCode::Esc);

    // The turn ends: the next one goes by itself.
    app.s().busy = false;
    app.drain_queues();
    assert_eq!(sent(&mut app), ["one"]);
    assert!(app.s().queue.is_empty() && app.s().busy, "and starts a turn");
}

#[test]
fn an_interrupt_holds_the_queue_and_send_now_stops_the_turn_and_goes_first() {
    let (mut app, _d) = fake_app("queue-now");
    app.s().busy = true;
    for m in ["a", "b"] {
        typed(&mut app, m);
        press(&mut app, KeyCode::Enter);
    }
    app.s().interrupt();
    app.s().busy = false; // the interrupted turn ended
    app.drain_queues();
    assert!(sent(&mut app).is_empty() && queued(&mut app) == ["a", "b"], "stopping Claude does not start the next message");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("⏸"), "the strip says it is paused: {text}");
    alt(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('p'));
    assert_eq!(sent(&mut app), ["a"], "p resumes");
    press(&mut app, KeyCode::Esc);

    // Send now: the chosen one jumps the line, the running turn is stopped, and it goes when that ends.
    app.s().queue.push(session::Queued { text: "c".into(), atts: vec![], due: None });
    alt(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert_eq!(queued(&mut app), ["c", "b"]);
    assert!(app.s().busy && !app.s().queue_hold, "the turn is being stopped; the rest are not held");
    press(&mut app, KeyCode::Esc);
    app.s().busy = false;
    app.drain_queues();
    assert_eq!(sent(&mut app), ["a", "c"]);

    // Steer: sent at once without stopping anything.
    alt(&mut app, KeyCode::Char('q'));
    app.s().busy = true;
    press(&mut app, KeyCode::Char('s'));
    assert_eq!(sent(&mut app), ["a", "c", "b"]);
}

#[test]
fn search_filters_the_conversation_as_you_type_and_tab_searches_what_you_sent() {
    let (mut app, _d) = fake_app("search");
    for i in 0..30 {
        app.s().push(Entry::User(format!("question {i}")));
        app.s().push(Entry::Assistant { text: format!("answer {i}{}", if i == 7 { " with the NEEDLE inside" } else { "" }), msg_id: format!("m{i}"), streaming: false });
    }
    app.history = vec!["old needle message".into(), "another".into()];
    app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    typed(&mut app, "needle");
    assert!(app.input.is_empty(), "the panel owns the keys");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("Search · 1 match") && text.contains("answer 7 with the NEEDLE"), "{text}");
    assert!(text.contains("question 6") || text.contains("answer 7"), "the conversation scrolled to the match");
    press(&mut app, KeyCode::Right);
    assert!(!app.search.as_ref().unwrap().history, "the matches have the keys: left and right do nothing");
    press(&mut app, KeyCode::Tab); // the scope strip
    press(&mut app, KeyCode::Right);
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("old needle message"), "{text}");
    press(&mut app, KeyCode::Enter);
    assert!(app.search.is_none() && app.input == "old needle message", "enter puts the old message in the box");

    // Esc puts the view back where it was.
    app.set_input(String::new());
    app.s().scroll = 5;
    app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    typed(&mut app, "answer 2");
    screen(&mut app, 100, 30);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.s().scroll, 5);
    // Enter stays at the match, and /find carries on from it.
    run_cmd(&mut app, "/search needle");
    screen(&mut app, 100, 30);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.s().find_query, "needle");
    assert!(app.s().found.is_some());
}

#[test]
fn esc_interrupts_a_running_turn_from_vim_insert_mode_when_nothing_is_typed() {
    let (mut app, _d) = fake_app("vim-interrupt");
    run_cmd(&mut app, "/vim");
    app.s().busy = true;
    typed(&mut app, "draft");
    press(&mut app, KeyCode::Esc);
    assert_eq!((app.vim.mode, app.s().busy), (vim::Mode::Normal, true), "text in the box: Esc is vim's, the draft is kept");
    assert_ne!(app.s().activity, "interrupting");
    app.set_input(String::new());
    app.vim.mode = vim::Mode::Insert;
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.s().activity, "interrupting", "empty box: Esc is the interrupt the spinner promises");
}

#[test]
fn vim_keys_edit_the_box_in_modes_and_sending_returns_to_insert() {
    let (mut app, _d) = fake_app("vim");
    run_cmd(&mut app, "/vim");
    assert!(app.vim.on);
    typed(&mut app, "hello big world");
    assert_eq!(app.input, "hello big world", "insert mode types");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.vim.mode, vim::Mode::Normal);
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("NORMAL"), "{text}");
    typed(&mut app, "0wdw");
    assert_eq!(app.input, "hello world");
    typed(&mut app, "x");
    assert_eq!(app.input, "hello orld", "x is not typed text");
    typed(&mut app, "u");
    assert_eq!(app.input, "hello world", "u undoes");
    app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(app.input, "hello orld", "ctrl+r redoes");
    typed(&mut app, "$ia");
    assert_eq!(app.input, "hello orlad");
    press(&mut app, KeyCode::Esc);
    typed(&mut app, "vbd");
    assert!(app.input.len() < "hello orlad".len(), "visual mode deletes the selection: {}", app.input);
    typed(&mut app, "Aok");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.vim.mode, vim::Mode::Insert, "after sending you are typing again");
    assert!(sent(&mut app)[0].ends_with("ok"));
    run_cmd(&mut app, "/vim");
    assert!(!app.vim.on);
    typed(&mut app, "dw");
    assert_eq!(app.input, "dw", "off again: keys are text");
}

#[test]
fn a_folder_that_brings_commands_of_its_own_does_not_start_claude_until_trusted() {
    let (mut app, d) = fake_app("trust-gate");
    let proj = std::env::temp_dir().join(format!("claudeplusplus-untrusted-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&proj);
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join(".mcp.json"), r#"{"mcpServers": {"from-the-repo": {"command": "sh"}}}"#).unwrap();
    app.sessions[0].cwd = Some(proj.to_str().unwrap().into());
    app.sessions[0].trust_gate = true; // off in tests, which use folders of their own
    typed(&mut app, "hello"); // typing would start claude ahead of the message
    press(&mut app, KeyCode::Enter);
    assert_eq!(spawns(&d), 0, "nothing was started in the untrusted folder");
    let why = app.sessions[0].items.iter().rev().find_map(|i| match &i.entry {
        session::Entry::Error(e) => Some(e.clone()),
        _ => None,
    });
    let why = why.expect("the refusal is shown");
    assert!(why.contains("Not started") && why.contains("from-the-repo") && why.contains("/trust"), "{why}");
    run_cmd(&mut app, "/trust");
    typed(&mut app, "again");
    press(&mut app, KeyCode::Enter);
    assert_eq!(wait_spawns(&d, 1), 1, "after /trust claude starts");
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn shared_mcp_hands_the_stdio_servers_to_claude_plus_plus_and_leaves_the_rest() {
    if !config::cache_dir().is_some_and(|d| std::fs::create_dir_all(&d).and_then(|_| std::fs::write(d.join(".probe"), "")).is_ok()) {
        return eprintln!("skipped: the cache directory cannot be written here");
    }
    let (mut app, _d) = fake_app("share-mcp");
    let dir = std::env::temp_dir().join(format!("claudeplusplus-share-cfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".mcp.json"), r#"{"mcpServers": {"cpp-test-stdio": {"command": "true"}, "cpp-test-remote": {"type": "http", "url": "https://example.test/mcp"}}}"#).unwrap();
    let cwd = dir.to_str().unwrap().to_string();
    app.sessions[0].cwd = Some(cwd.clone());
    let off = app.launch_for(0);
    assert!(off.shared.is_empty() && !off.args.contains(&"--strict-mcp-config".to_string()), "off by default: claude starts its own servers");
    assert_eq!(app.setting_value("share_mcp"), "off");
    app.apply_setting("share_mcp", "on");
    assert_eq!(app.setting_value("share_mcp"), "on");
    let on = app.launch_for(0);
    let mine = on.shared.iter().find(|s| s.0 == "cpp-test-stdio").expect("the stdio server is hosted");
    assert_eq!((mine.1.command.as_str(), mine.1.cwd.as_str()), ("true", cwd.as_str()), "started in the tab's folder");
    assert!(!on.shared.iter().any(|s| s.0 == "cpp-test-remote"), "a remote server costs no process: claude keeps it");
    assert!(on.args.contains(&"--strict-mcp-config".to_string()), "so claude does not start its own copy too");
    let file = &on.args[on.args.iter().position(|a| a == "--mcp-config").unwrap() + 1];
    let rest = std::fs::read_to_string(file).unwrap();
    assert!(rest.contains("cpp-test-remote") && !rest.contains("cpp-test-stdio"), "{rest}");
    // A profile that already chose its MCP servers (lean has none) is left alone.
    app.sessions[0].profile = "lean".into();
    assert!(app.launch_for(0).shared.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_turns_vim_keys_and_the_message_queue_on_and_off() {
    let (mut app, _d) = fake_app("cfg-vim");
    let cfg = std::env::temp_dir().join(format!("claudeplusplus-cfg-vim-{}", std::process::id())).join("config.json");
    let _ = std::fs::remove_file(&cfg);
    app.config_path = Some(cfg.clone());
    run_cmd(&mut app, "/config");
    press(&mut app, KeyCode::Right); // General opens first: its first row is Input mode
    assert!(app.vim.on, "the Input mode row: default → vim");
    assert_eq!(config::get_str(&cfg, "input_mode").as_deref(), Some("vim"));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right);
    assert!(!app.queue_on, "the While Claude works row: send at once");
    assert_eq!(config::get_str(&cfg, "queue").as_deref(), Some("off"));
    press(&mut app, KeyCode::Esc);
    app.s().busy = true;
    typed(&mut app, "now");
    press(&mut app, KeyCode::Enter);
    assert!(app.s().queue.is_empty() && sent(&mut app) == ["now"], "with the queue off it goes straight out");
    run_cmd(&mut app, "/vim");
    assert!(!app.vim.on);
    assert_eq!(config::get_str(&cfg, "input_mode").as_deref(), Some("default"), "/vim and /config are the same switch");
}

#[test]
fn ctrl_g_asks_for_the_editor_and_the_edited_text_comes_back_without_the_editors_last_newline() {
    let (mut app, _d) = fake_app("editor");
    typed(&mut app, "draft");
    assert!(!app.edit_request);
    app.on_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
    assert!(app.edit_request, "the main loop hands over the terminal");
    assert_eq!(app.input, "draft");

    assert_eq!(edit_text("printf ' more\\nlines\\n' >>", "draft").unwrap(), "draft more\nlines");
    assert_eq!(edit_text("true", "kept\n\n").unwrap(), "kept\n", "only the editor's own final newline goes");
    let err = edit_text("false", "x").unwrap_err().to_string();
    assert!(err.contains("false"), "a failing editor is reported: {err}");
    assert!(edit_text("no-such-editor-claudepp", "x").is_err());
}

#[test]
fn a_long_line_of_unmatched_markers_renders_in_linear_time() {
    let line = "a*b _c ~d ".repeat(20_000) + &"http://x.io ".repeat(5_000);
    let t = std::time::Instant::now();
    let _ = markdown::render(&line, 100);
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn export_writes_the_conversation_and_bare_mode_opens_the_tab_settings() {
    let (mut app, dir) = fake_app("export");
    app.s().cwd = Some(dir.to_string_lossy().into());
    run_cmd(&mut app, "/export");
    assert_eq!(app.flash.as_deref(), Some("nothing to export yet"));
    app.s().push(Entry::User("fix it".into()));
    app.s().push(Entry::Tool { id: "t".into(), name: "Bash".into(), input: json!({"command": "ls"}), result: Some(("a\nb".into(), false)), nested: false });
    app.s().push(Entry::Assistant { text: "done".into(), msg_id: "m".into(), streaming: false });
    run_cmd(&mut app, "/export out.txt");
    let text = std::fs::read_to_string(dir.join("out.txt")).unwrap();
    assert_eq!(text, "> fix it\n\n● Bash({\"command\":\"ls\"})\n  ⎿ a\n  ⎿ b\n\ndone\n\n");
    run_cmd(&mut app, "/mode");
    assert!(app.tabcfg.is_some());
    run_cmd(&mut app, "/memory");
    assert_eq!(app.edit_file, Some(dir.join("CLAUDE.md")));
}

#[test]
fn a_code_span_does_not_hide_emphasis_after_it() {
    let text: String = markdown::render("*x `a *b*", 80).iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
    assert!(!text.contains("*b*"), "{text}");
}

#[test]
fn ctrl_r_searches_what_you_sent_and_lists_the_latest_first() {
    let (mut app, _d) = fake_app("ctrlr");
    app.history = vec!["first prompt".into(), "fix the parser".into(), "run tests".into()];
    app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
    let ui = app.search.as_ref().unwrap();
    assert!(ui.history);
    assert_eq!(ui.hits.iter().map(|h| h.text.2.as_str()).collect::<Vec<_>>(), ["run tests", "fix the parser", "first prompt"]);
    typed(&mut app, "pars");
    assert_eq!(app.search.as_ref().unwrap().hits.len(), 1);
}

#[test]
fn ctrl_b_backgrounds_the_running_command_and_tasks_lists_and_stops_it() {
    let (mut app, _d) = fake_app("bg");
    let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
    app.on_key(ctrl_b);
    assert_eq!(app.flash.as_deref(), Some("no command running in the foreground"));
    app.s().busy = true;
    app.s().handle(&json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m", "content": [{"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "make"}}]}}));
    app.on_key(ctrl_b);
    assert!(app.flash.as_deref().unwrap().contains("only just started"), "{:?}", app.flash);
    app.s().handle(&json!({"type": "system", "subtype": "task_started", "task_id": "b1", "tool_use_id": "tu1", "task_type": "local_bash", "description": "Build", "is_backgrounded": false}));
    app.on_key(ctrl_b);
    assert!(app.flash.as_deref().unwrap().starts_with("moved to the background"));
    assert!(app.s().shells[0].background);
    assert!(app.s().deferred.iter().any(|(k, f)| k == "background_tasks" && f["tool_use_id"] == "tu1"));
    assert!(!app.s().can_suspend(), "stopping the process would kill the command");

    run_cmd(&mut app, "/tasks");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("Build") && text.contains("x stop"), "{text}");
    app.on_key(key('x'));
    assert!(app.s().deferred.iter().any(|(k, f)| k == "stop_task" && f["task_id"] == "b1"));
    app.s().handle(&json!({"type": "system", "subtype": "task_notification", "task_id": "b1", "tool_use_id": "tu1", "status": "stopped", "summary": "Build"}));
    assert_eq!(app.s().shells[0].status.as_deref(), Some("stopped"));
}

#[test]
fn thinking_shows_as_one_folded_line_and_ctrl_o_reads_it() {
    let (mut app, _d) = fake_app("think");
    app.s().handle(&json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m", "content": [{"type": "thinking", "thinking": "The parser drops the last token."}]}}));
    app.s().handle(&json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m", "content": [{"type": "thinking", "thinking": ""}]}}));
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("✻ Thinking…") && !text.contains("parser drops"), "{text}");
    assert_eq!(text.matches("Thinking").count(), 1, "empty (redacted) thinking adds nothing");
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("The parser drops the last token."), "{text}");
}

#[test]
#[ignore]
fn zz_long_session_memory() {
    let rss = || {
        let o = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().unwrap() / 1024
    };
    let (mut app, _d) = fake_app("mem");
    let before = rss();
    let body: String = (0..400).map(|i| format!("line {i} of some tool output that is fairly typical\n")).collect();
    let reply = "## Heading\n\nSome **bold** text and `code` and a list:\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n".repeat(20);
    for i in 0..3000 {
        app.s().push(Entry::Tool { id: format!("t{i}"), name: "Bash".into(), input: json!({"command": "cat x"}), result: Some((body.clone(), false)), nested: false });
        app.s().push(Entry::Assistant { text: reply.clone(), msg_id: format!("m{i}"), streaming: false });
    }
    let stored = rss();
    // Scroll through everything so every entry gets wrapped once.
    for top in (0..300).map(|k| k * 10) {
        app.s().scroll = top * 40;
        let _ = screen(&mut app, 120, 40);
    }
    let kept = |app: &mut App| app.s().items.iter().filter(|i| i.cache.as_ref().is_some_and(|c| !c.2.is_empty())).count();
    let t = std::time::Instant::now();
    let _ = screen(&mut app, 119, 40);
    let scrolled = t.elapsed();
    app.s().scroll = 0;
    let _ = screen(&mut app, 119, 40);
    let t = std::time::Instant::now();
    let _ = screen(&mut app, 118, 40);
    eprintln!(
        "rss MB: start {before}, after storing {stored}, after rendering {}; entries holding lines {} of {}; resize frame scrolled up {scrolled:?}, following {:?}",
        rss(),
        kept(&mut app),
        app.s().items.len(),
        t.elapsed()
    );
    assert!(kept(&mut app) < 1000);
}

#[test]
fn the_first_message_waits_for_mcp_servers_still_connecting_and_esc_drops_it() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\ncat >> {}/stdin\n", dir.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let launch = Launch { bin: script.to_string_lossy().into(), ..Default::default() };
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(launch.clone(), "/".into(), tx.clone());
    app.new_tab();
    let stdin = || std::fs::read_to_string(dir.join("stdin")).unwrap_or_default();
    let users = || stdin().lines().filter(|l| l.contains(r#""type":"user""#)).count();
    let s = &mut app.sessions[0];
    s.warm(&launch, &tx);
    s.mcp_wait = Some(Instant::now()); // as outside tests: a fresh backend whose servers may be connecting
    s.submit("first".into(), &[], &launch, &tx);
    let status = |state: &str| serde_json::from_str::<serde_json::Value>(&format!(r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"cpp-9","response":{{"mcpServers":[{{"name":"a","status":"connected"}},{{"name":"b","status":"{state}"}}]}}}}}}"#)).unwrap();
    s.handle(&status("pending"));
    thread::sleep(Duration::from_millis(300));
    assert!(stdin().contains("mcp_status") && users() == 0 && s.activity == "connecting MCP servers", "held while a server connects: {}", stdin());
    s.handle(&status("failed"));
    let sent = (0..60).any(|_| {
        thread::sleep(Duration::from_millis(50));
        users() == 1
    });
    assert!(sent && s.mcp_wait.is_none(), "sent once every server settled, failed ones included");

    // A respawned backend waits again; esc while waiting drops the message instead of interrupting.
    s.mcp_wait = Some(Instant::now());
    s.busy = false;
    s.submit("second".into(), &[], &launch, &tx);
    s.interrupt();
    s.mcp_wait = Some(Instant::now() - Duration::from_secs(6));
    s.poll_mcp();
    thread::sleep(Duration::from_millis(300));
    assert!(users() == 1 && !s.busy && !stdin().contains("interrupt"), "{}", stdin());
}

#[test]
fn a_scheduled_message_waits_for_its_time_shares_the_queue_and_does_not_get_held_by_an_interrupt() {
    let (mut app, _d) = fake_app("schedule");
    // Not a complete command: a list of times, then the message goes in the ordinary box.
    typed(&mut app, "/schedule");
    press(&mut app, KeyCode::Enter);
    assert!(app.sched_ui.is_some());
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("Schedule") && text.contains("in 15 minutes") && text.contains("next 9:00"), "{text}");
    typed(&mut app, "soon");
    press(&mut app, KeyCode::Enter);
    assert!(app.sched_ui.is_some() && app.sched_at.is_none(), "a time that is not one is refused");
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert!(app.sched_ui.is_none() && app.sched_at.is_some(), "picked: in 1 hour");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("scheduled for") && text.contains("esc cancels"), "{text}");
    typed(&mut app, "check the build");
    press(&mut app, KeyCode::Enter);
    assert!(app.sched_at.is_none());
    assert_eq!(queued(&mut app), ["check the build"]);
    let due = app.s().queue[0].due.unwrap().duration_since(SystemTime::now()).unwrap().as_secs();
    assert!((3590..=3600).contains(&due), "{due}");
    // Esc before typing drops the schedule.
    typed(&mut app, "/schedule 2h");
    press(&mut app, KeyCode::Enter);
    assert!(app.sched_at.is_some() && app.sched_ui.is_none());
    press(&mut app, KeyCode::Esc);
    assert!(app.sched_at.is_none() && queued(&mut app).len() == 1);
    // A complete command needs nothing more.
    typed(&mut app, "/schedule 1h later one");
    press(&mut app, KeyCode::Enter);
    assert!(app.sched_ui.is_none() && queued(&mut app) == ["check the build", "later one"]);
    app.s().queue.pop();
    app.drain_queues();
    assert!(sent(&mut app).is_empty() && !app.s().busy, "not yet");
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("@") && text.contains("check the build"), "{text}");
    // An ordinary message does not wait behind it.
    typed(&mut app, "now please");
    press(&mut app, KeyCode::Enter);
    assert_eq!(sent(&mut app), ["now please"]);
    assert_eq!(queued(&mut app), ["check the build"]);
    // Stopping Claude holds ordinary queued messages, not the scheduled one.
    app.s().interrupt();
    assert!(!app.s().queue_hold);
    // Its time comes: it goes once the tab is free.
    app.s().queue[0].due = Some(SystemTime::now() - Duration::from_secs(1));
    app.s().busy = false;
    app.tick();
    assert_eq!(sent(&mut app), ["now please", "check the build"]);
}

#[test]
fn a_queued_message_gets_a_time_from_the_queue_panel_and_loses_it_with_an_empty_one() {
    let (mut app, _d) = fake_app("queue-schedule");
    app.s().busy = true;
    typed(&mut app, "later");
    press(&mut app, KeyCode::Enter);
    assert!(app.s().queue[0].due.is_none());
    alt(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('S'));
    assert!(app.queue_ui.is_none() && app.sched_ui.is_some());
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("later") && text.contains("Schedule"), "the queued message is shown: {text}");
    typed(&mut app, "5m");
    press(&mut app, KeyCode::Enter);
    assert!(app.sched_ui.is_none() && app.queue_ui.is_some(), "back to the queue");
    assert!(app.s().queue[0].due.is_some());
    press(&mut app, KeyCode::Char('S'));
    press(&mut app, KeyCode::Enter);
    assert!(app.s().queue[0].due.is_none(), "the first row, no time, takes it off");
}

// ---- hostile backend: nothing the other side sends may panic the UI -------------------------------------------

/// Every `{...}` JSON message written as a raw string in our own tests: realistic seeds that stay current.
pub(crate) fn seed_messages() -> Vec<Value> {
    let mut out = vec![];
    for src in [include_str!("session.rs"), include_str!("session/tests.rs"), include_str!("tests.rs")] {
        let mut rest = src;
        while let Some(i) = rest.find("r#\"{") {
            rest = &rest[i + 3..];
            let Some(end) = rest.find("\"#") else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&rest[..end])
                && v.get("type").is_some()
            {
                out.push(v);
            }
            rest = &rest[end..];
        }
    }
    out
}

pub(crate) struct Rng(pub u64);
impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn hostile_value(r: &mut Rng) -> Value {
    match r.below(14) {
        0 => Value::Null,
        1 => json!(0),
        2 => json!(-1),
        3 => json!(1e308),
        4 => json!(u64::MAX),
        5 => json!(""),
        6 => json!("x".repeat(20_000)),
        7 => json!("a\u{0}b\u{200b}\u{202e}e\u{301}\u{1F468}\u{200d}\u{1F469}\u{fe0f}\t\r\n\x1b[31m\x1b]0;t\x07"),
        8 => json!([]),
        9 => json!({}),
        10 => json!([null, [], {"a": 1}]),
        11 => json!(true),
        12 => json!("日本語のとても長い行".repeat(50)),
        _ => json!({"type": "text", "text": "nested", "content": [1, 2, 3]}),
    }
}

/// Apply one mutation at a random place inside `v`.
pub(crate) fn mutate(v: &mut Value, r: &mut Rng) {
    match v {
        Value::Object(m) if !m.is_empty() => {
            let keys: Vec<String> = m.keys().cloned().collect();
            let k = keys[r.below(keys.len())].clone();
            match r.below(5) {
                0 => {
                    m.remove(&k);
                }
                1 => {
                    m.insert(k, hostile_value(r));
                }
                _ => mutate(m.get_mut(&k).unwrap(), r),
            }
        }
        Value::Array(a) if !a.is_empty() => {
            let i = r.below(a.len());
            match r.below(4) {
                0 => {
                    a.remove(i);
                }
                1 => a[i] = hostile_value(r),
                _ => mutate(&mut a[i], r),
            }
        }
        Value::String(s) if r.below(2) == 0 => {
            let cut = s.chars().take(r.below(s.chars().count() + 1)).collect::<String>();
            *s = cut;
        }
        _ => *v = hostile_value(r),
    }
}

#[test]
fn hostile_backend_messages_and_keys_never_panic_the_ui() {
    let seeds = seed_messages();
    assert!(seeds.len() > 40, "seeds are read from our own tests: {}", seeds.len());
    // A deep run: FUZZ_ROUNDS=100 FUZZ_SEED=7 cargo test --release hostile_backend -- --nocapture
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let mut r = Rng(env("FUZZ_SEED", 0x9E37_79B9_7F4A_7C15) | 1);
    let keys = [KeyCode::Enter, KeyCode::Esc, KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right, KeyCode::Tab,
        KeyCode::Char('1'), KeyCode::Char('2'), KeyCode::Char('y'), KeyCode::Char('n'), KeyCode::Char('a'), KeyCode::Char(' '), KeyCode::Char('x')];
    for round in 0..env("FUZZ_ROUNDS", 2) {
        let (mut app, _d) = fake_app(&format!("hostile-{round}"));
        let mut log: Vec<String> = vec![];
        for step in 0..1500 {
            let mut m = seeds[r.below(seeds.len())].clone();
            for _ in 0..1 + r.below(3) {
                mutate(&mut m, &mut r);
            }
            log.push(m.to_string().chars().take(300).collect());
            if log.len() > 6 {
                log.remove(0);
            }
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                app.sessions[0].handle(&m);
                if step % 25 == 7 {
                    // Panels that only read: the rest write settings files, which a random key must never reach.
                    const PANELS: [&str; 15] = ["/context", "/usage", "/tasks", "/procs", "/sessions", "/help", "/model", "/effort", "/mode", "/session", "/find a", "/search", "/queue", "/color", "/config"];
                    run_cmd(&mut app, PANELS[r.below(PANELS.len())]);
                }
                if r.below(3) == 0 {
                    let k = keys[r.below(keys.len())];
                    app.on_key(KeyEvent::new(k, KeyModifiers::NONE));
                }
                if step % 20 == 0 {
                    for (w, h) in [(100, 30), (24, 8), (1, 1), (5, 3), (14, 40), (300, 4)] {
                        screen(&mut app, w, h);
                    }
                }
            }));
            if res.is_err() {
                panic!("round {round} step {step}: a hostile message panicked the UI. Last messages:\n{}", log.join("\n"));
            }
        }
    }
}

// ---- the editor pane (input mode "editor", ctrl+g full screen) ------------------------------------------------
// These need a pty; where one cannot be opened (a sandbox) they say so and pass.

fn pane_app(editor: &str) -> (App, mpsc::Receiver<AppEvent>) {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(Launch::default(), "/".into(), tx);
    app.new_tab();
    app.editor_cmd = Some(editor.into());
    (app, rx)
}

/// Deliver the editor's output and its end, as the main loop would, until the pane is gone
/// (without `tick`, which in input mode editor would open the next one).
fn run_pane_to_the_end(app: &mut App, rx: &mpsc::Receiver<AppEvent>) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while app.pane.is_some() && Instant::now() < end {
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(50)) {
            app.on_event(ev);
        }
        if app.pane.as_mut().is_some_and(|p| p.exited()) {
            app.finish_pane();
        }
    }
    app.pane.is_none()
}

#[test]
fn input_mode_editor_is_open_without_a_key_sends_what_the_editor_leaves_and_comes_back() {
    let (mut app, rx) = pane_app("printf 'hello from the editor' >");
    app.apply_setting("input_mode", "editor");
    assert_eq!(app.setting_value("input_mode"), "editor");
    app.tick();
    if app.pane.is_none() {
        return eprintln!("skipped: no pty here");
    }
    assert!(run_pane_to_the_end(&mut app, &rx), "the editor ended");
    assert_eq!(sent(&mut app), ["hello from the editor"]);
    assert!(app.input.is_empty());
    app.tick();
    assert!(app.pane.is_some(), "a fresh editor replaces it after a send");
    app.apply_setting("input_mode", "default");
    app.tick();
    assert!(app.pane.is_none(), "turning the mode off closes it");
}

#[test]
fn a_failing_editor_sends_nothing_and_keeps_the_text_and_ctrl_backslash_cancels() {
    let (mut app, rx) = pane_app("printf 'draft' > \"$1\"; exit 3 #");
    app.apply_setting("input_mode", "editor");
    app.tick();
    if app.pane.is_none() {
        return eprintln!("skipped: no pty here");
    }
    assert!(run_pane_to_the_end(&mut app, &rx));
    assert!(sent(&mut app).is_empty(), "exit status 3 (:cq) is not a send");
    assert_eq!(app.input, "draft", "what was written is not thrown away");

    let (mut app, _rx) = pane_app("sleep 30 #");
    app.apply_setting("input_mode", "editor");
    app.tick();
    assert!(app.pane.is_some());
    app.on_key(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL));
    assert!(app.pane.is_none() && sent(&mut app).is_empty(), "ctrl+\\ closes it without sending");
}

#[test]
fn a_new_session_asks_for_the_cache_friendly_prompt_unless_switched_off() {
    let flag = "--exclude-dynamic-system-prompt-sections".to_string();
    let (mut app, _rx) = pane_app("sleep 1 #");
    assert!(app.launch_for(0).args.contains(&flag), "on by default: cwd and git status move out of the cached system prompt");
    app.stable_prompt = false;
    assert!(!app.launch_for(0).args.contains(&flag), "\"stable_prompt\": \"off\" in config.json turns it off");
}

#[test]
fn compaction_size_is_passed_on_and_a_very_long_conversation_is_told_about_it_once() {
    let (mut app, _rx) = pane_app("sleep 1 #");
    assert!(!app.launch_for(0).args.contains(&"--autocompact".to_string()), "off by default: Claude Code's own size");
    app.apply_setting("autocompact_k", "200");
    assert_eq!(app.setting_value("autocompact_k"), "200");
    assert!(app.launch_for(0).args.windows(2).any(|w| w == ["--autocompact", "200k"]));
    app.apply_setting("autocompact_k", "0");
    app.sessions[0].ctx_tokens = 450_000;
    app.on_mem();
    app.on_mem();
    let notes = |a: &App| a.sessions[0].items.iter().filter(|i| matches!(&i.entry, session::Entry::Note(t) if t.contains("450k tokens"))).count();
    assert_eq!(notes(&app), 1, "said once");
    app.sessions[0].long_hinted = false;
    app.apply_setting("autocompact_k", "300");
    app.on_mem();
    assert_eq!(notes(&app), 1, "not at all once a size is chosen");
}

#[test]
fn claude_plus_plus_keys_still_work_with_the_editor_open() {
    let (mut app, _rx) = pane_app("sleep 30 #");
    app.history = vec!["old one".into(), "old two".into()];
    app.apply_setting("input_mode", "editor");
    app.tick();
    if app.pane.is_none() {
        return eprintln!("skipped: no pty here");
    }
    let id = app.pane.as_ref().unwrap().id;
    // Untouched editor: ↑ ↓ walk history, and the message becomes the editor's text (a fresh editor).
    press(&mut app, KeyCode::Up);
    assert_eq!(app.hist_pos, Some(1));
    assert!(app.input.is_empty(), "the text moved into the editor");
    assert!(app.pane.as_ref().is_some_and(|p| p.id != id), "a new editor holds it");
    press(&mut app, KeyCode::Up);
    assert_eq!(app.hist_pos, Some(0));
    press(&mut app, KeyCode::Down);
    assert_eq!(app.hist_pos, Some(1));
    // Chords no editor needs, and the alt twins of the ones it does.
    press(&mut app, KeyCode::PageUp);
    assert!(app.sessions[app.active].scroll > 0, "pgup scrolls the conversation");
    app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT));
    assert!(app.search.is_some(), "alt+f is ctrl+f");
    app.search = None;
    // ← on an untouched editor opens the session list.
    press(&mut app, KeyCode::Left);
    assert!(app.explorer.is_some() || app.dock.is_some(), "← goes to the sessions");
    app.explorer = None;
    app.dock = None;
    // Once you type, the editor owns ↑ ↓ ← esc.
    typed(&mut app, "x");
    assert!(app.pane_typed);
    let at = app.hist_pos;
    for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Esc] {
        press(&mut app, code);
    }
    assert_eq!(app.hist_pos, at, "history untouched");
    assert!(app.explorer.is_none() && app.dock.is_none(), "no session list");
    // ctrl+z would suspend the editor: it is not passed on.
    app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert!(app.pane.is_some());
}

#[test]
fn ctrl_g_edits_the_box_in_a_full_screen_editor_and_puts_the_text_back() {
    let (mut app, rx) = pane_app("printf 'edited text' >");
    app.set_input("old text".into());
    if app.open_pane(pane::Use::Edit, "old text").is_err() {
        return eprintln!("skipped: no pty here");
    }
    app.pane_old = Some(("old text".into(), 8));
    assert!(run_pane_to_the_end(&mut app, &rx));
    assert_eq!(app.input, "edited text");
    assert!(sent(&mut app).is_empty(), "ctrl+g edits; it does not send");
    app.on_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_eq!(app.input, "old text", "and it is one undo away");
}

#[test]
fn the_editor_pane_draws_at_any_terminal_size() {
    for kind in [pane::Use::Send, pane::Use::Edit] {
        let (mut app, _rx) = pane_app("sleep 30 #");
        if app.open_pane(kind, "some text").is_err() {
            return eprintln!("skipped: no pty here");
        }
        for (w, h) in [(100, 30), (20, 6), (1, 1), (7, 40), (200, 3)] {
            screen(&mut app, w, h);
        }
        let rows = screen(&mut app, 100, 30);
        if kind == pane::Use::Send {
            assert!(rows.iter().any(|r| r.contains("quit it to send")), "{rows:?}");
        } else {
            assert!(rows.last().is_some_and(|r| r.contains("ctrl+\\ cancels")), "{rows:?}");
        }
    }
}

#[test]
fn a_big_idle_conversation_is_compacted_in_the_last_seconds_of_its_cache_but_not_while_you_type() {
    let (mut app, _d) = fake_app("precompact");
    let (launch, tx) = (app.launch.clone(), app.tx.clone());
    let sent = |a: &App| a.sessions[0].items.iter().filter(|i| matches!(&i.entry, session::Entry::User(t) if t == "/compact")).count();
    let ttl = app.cache_ttl;
    let age = |s: u64| Instant::now().checked_sub(ttl - Duration::from_secs(s));
    app.sessions[0].warm(&launch, &tx);
    app.sessions[0].ctx_tokens = 150_000;
    app.sessions[0].last_api = age(20);
    app.tick();
    assert_eq!(sent(&app), 0, "off by default");
    app.apply_setting("precompact_k", "200");
    app.tick();
    assert_eq!(sent(&app), 0, "a conversation under the size is left alone");
    app.apply_setting("precompact_k", "100");
    app.sessions[0].last_api = age(300);
    app.tick();
    assert_eq!(sent(&app), 0, "the cache has minutes left: the user may come back");
    app.sessions[0].last_api = age(20);
    app.input = "half a thought".into();
    app.tick();
    assert_eq!(sent(&app), 0, "the tab being typed in is not interrupted");
    app.input.clear();
    app.tick();
    assert_eq!(sent(&app), 1, "idle, big, cache about to expire");
    assert!(app.sessions[0].busy && app.sessions[0].precompacted);
    app.sessions[0].busy = false;
    app.tick();
    assert_eq!(sent(&app), 1, "once per cache window");
}

#[test]
fn the_builtin_status_line_shows_the_parts_you_chose_in_the_order_you_chose_and_remembers_them() {
    let (mut app, d) = fake_app("items");
    let cfg = d.join("config.json");
    app.config_path = Some(cfg.clone());
    app.sessions[0].ctx_tokens = 5000;
    let line = |app: &mut App| screen(app, 120, 20).into_iter().rev().find(|r| r.contains("mem ")).unwrap_or_default();
    let l = line(&mut app);
    assert!(l.find("ctx 5").unwrap() < l.find("mem ").unwrap(), "default order: {l:?}");

    run_cmd(&mut app, "/statusline items");
    assert!(app.items_ui.is_some());
    // ctx is row 2 (profile, model, ctx): hide it, then move mem (now row 6) to the front.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char(' '));
    assert!(!line(&mut app).contains("ctx 5"), "hidden at once: {:?}", line(&mut app));
    assert_eq!(config::get_str(&cfg, "statusline_items").as_deref(), Some("profile,model,hit,cache,cost,limits,mem"));
    app.items_ui.as_mut().unwrap().sel = 6;
    for _ in 0..6 {
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    }
    assert_eq!(app.line_items[0], "mem");
    app.items_ui.as_mut().unwrap().sel = rows_of(&app).iter().position(|r| *r == "clock").unwrap();
    press(&mut app, KeyCode::Char(' '));
    assert!(app.line_items.contains(&"clock") && line(&mut app).contains(':'), "a part that is off by default can be added");
    press(&mut app, KeyCode::Esc);
    assert!(app.items_ui.is_none());
    assert!(config::get_str(&cfg, "statusline_items").unwrap().starts_with("mem,profile,model"));

    // what was saved comes back; `r` in the panel resets to the default
    app.line_items = segments::parse(&config::get_str(&cfg, "statusline_items").unwrap());
    assert_eq!(app.line_items[0], "mem");
    run_cmd(&mut app, "/statusline items");
    press(&mut app, KeyCode::Char('r'));
    assert_eq!(app.line_items, segments::DEFAULT);
}

fn rows_of(app: &App) -> Vec<&'static str> {
    segments::rows(&app.line_items)
}

#[test]
fn version_lists_what_is_installed_and_pins_one_for_new_and_restarted_tabs() {
    let (mut app, d) = fake_app("version");
    let vdir = d.join("versions");
    std::fs::create_dir_all(&vdir).unwrap();
    for v in ["2.1.5", "2.1.6", "2.1.10"] {
        std::fs::write(vdir.join(v), "x").unwrap();
    }
    app.versions_dir = Some(vdir.clone());
    let cfg = d.join("config.json");
    app.config_path = Some(cfg.clone());
    let last_note = |a: &App| a.sessions[0].items.iter().rev().find_map(|i| if let session::Entry::Note(t) = &i.entry { Some(t.clone()) } else { None }).unwrap_or_default();
    run_cmd(&mut app, "/version");
    let list = last_note(&app);
    assert!(list.find("2.1.6").unwrap() < list.find("2.1.10").unwrap() && list.contains("2.1.10  (newest, verified line)"), "{list}");

    let (launch, tx) = (app.launch.clone(), app.tx.clone());
    app.sessions[0].warm(&launch, &tx);
    assert!(app.sessions[0].backend.is_some());
    app.launch.bin = "claude".into(); // the fake script stood in for it; from here on it is the plain command
    run_cmd(&mut app, "/version 9.9.9");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("not installed")) && app.pin.is_none());

    run_cmd(&mut app, "/version 2.1.6");
    let want = vdir.join("2.1.6").to_string_lossy().into_owned();
    let l = app.launch_for(0);
    assert_eq!(l.bin, want, "the pinned file is what runs");
    assert!(l.env.contains(&("DISABLE_AUTOUPDATER".into(), "1".into())), "and it does not update itself");
    assert_eq!(config::get_str(&cfg, "claude_version").as_deref(), Some("2.1.6"));
    assert!(app.sessions[0].backend.is_none(), "an idle tab restarts on the pinned version with its next message");
    assert!(last_note(&app).contains("pinned to 2.1.6") && last_note(&app).contains("1 idle tab"));

    run_cmd(&mut app, "/version default");
    let l = app.launch_for(0);
    assert_eq!(l.bin, "claude");
    assert!(!l.env.iter().any(|(k, _)| k == "DISABLE_AUTOUPDATER"));
    assert_eq!(config::get_str(&cfg, "claude_version").as_deref(), Some(""));

    app.launch.bin = "/opt/my-claude".into();
    run_cmd(&mut app, "/version 2.1.5");
    assert!(app.pin.is_none() && app.flash.as_deref().is_some_and(|f| f.contains("--claude-bin")), "an explicit executable wins");
}

#[test]
fn cd_moves_the_tab_to_another_folder_and_its_backend_restarts_there() {
    let (mut app, d) = fake_app("cd");
    let (a, b) = (d.join("a"), d.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(b.join("sub")).unwrap();
    std::fs::write(d.join("file.txt"), "x").unwrap();
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap().to_string_lossy().into_owned();
    app.sessions[0].cwd = Some(canon(&a));
    let (launch, tx) = (app.launch.clone(), app.tx.clone());
    app.sessions[0].warm(&launch, &tx);
    assert!(app.sessions[0].backend.is_some());

    run_cmd(&mut app, &format!("/cd {}", b.display()));
    assert_eq!(app.cwd(), canon(&b));
    assert!(app.sessions[0].backend.is_none(), "restarted so the next message resumes in the new folder");
    assert_eq!(app.launch_for(0).cwd.as_deref(), Some(canon(&b).as_str()));
    run_cmd(&mut app, "/cd sub");
    assert_eq!(app.cwd(), canon(&b.join("sub")), "relative to where the tab is");
    run_cmd(&mut app, "/cd ..");
    assert_eq!(app.cwd(), canon(&b));
    run_cmd(&mut app, "/cd -");
    assert_eq!(app.cwd(), canon(&b.join("sub")), "- goes back");
    run_cmd(&mut app, "/cd nope");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("not a folder")) && app.cwd() == canon(&b.join("sub")));
    run_cmd(&mut app, &format!("/cd {}", d.join("file.txt").display()));
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("not a folder")), "a file is not a folder");
    app.sessions[0].busy = true;
    run_cmd(&mut app, &format!("/cd {}", a.display()));
    assert_eq!(app.cwd(), canon(&b.join("sub")), "a tab mid-turn stays where it is");
    app.sessions[0].busy = false;
    run_cmd(&mut app, "/cd");
    assert!(matches!(&app.sessions[0].items.last().unwrap().entry, session::Entry::Note(t) if t.contains("working folder:")));
}

#[test]
fn the_shell_setting_picks_login_or_sh() {
    let (mut app, _d) = fake_app("shell");
    assert_eq!(app.setting_value("shell"), "login");
    app.apply_setting("shell", "sh");
    assert!(!app.shell_login && app.setting_value("shell") == "sh");
    app.apply_setting("shell", "login");
    assert!(app.shell_login);
}

#[test]
fn open_lists_links_and_files_opens_them() {
    let (mut app, d) = fake_app("open");
    let dir = d.join("proj");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "x").unwrap();
    app.sessions[0].cwd = Some(dir.to_string_lossy().into());
    app.sessions[0].push(session::Entry::Assistant { text: "Docs at https://example.com/guide and the code in src/lib.rs:7.".into(), msg_id: String::new(), streaming: false });
    open::OPENED.lock().unwrap().clear();
    let opened = || open::OPENED.lock().unwrap().clone();

    run_cmd(&mut app, "/open");
    assert_eq!(app.open_ui.as_ref().unwrap().found.len(), 2);
    let rows = screen(&mut app, 100, 24);
    assert!(rows.iter().any(|r| r.starts_with(" Open")) && rows.iter().any(|r| r.contains("https://example.com/guide")) && rows.iter().any(|r| r.contains("src/lib.rs:7")), "{}", rows.join("\n"));
    press(&mut app, KeyCode::Enter);
    assert!(app.open_ui.is_none() && opened().last().unwrap().last().unwrap() == "https://example.com/guide", "{:?}", opened());

    run_cmd(&mut app, "/open 2");
    assert!(opened().last().unwrap().last().unwrap().ends_with("src/lib.rs"));
    run_cmd(&mut app, "/open ftp://nope");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("nothing to open")));
    let n = opened().len();
    run_cmd(&mut app, "/open");
    press(&mut app, KeyCode::Down);
    app.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
    assert_eq!(app.edit_file.as_deref(), Some(dir.join("src/lib.rs").as_path()), "e edits the file here");
    assert_eq!(opened().len(), n, "and opens nothing outside");
}

#[test]
fn worktree_adds_opens_a_tab_in_the_new_folder_lists_and_removes_it_after_a_second_d() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return eprintln!("skipped: no git");
    }
    let dir = std::env::temp_dir().join(format!("claudeplusplus-wt-app-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = dir.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    let g = |args: &[&str]| assert!(std::process::Command::new("git").current_dir(&repo).args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "init.defaultBranch=main"]).args(args).output().unwrap().status.success());
    g(&["init", "-q"]);
    std::fs::write(repo.join("f"), "x").unwrap();
    g(&["add", "."]);
    g(&["commit", "-q", "-m", "one"]);
    let repo = std::fs::canonicalize(&repo).unwrap();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(Launch::default(), repo.to_string_lossy().into(), tx);
    app.new_tab();
    let next = |app: &mut App| {
        let ev = (0..50).find_map(|_| rx.recv_timeout(Duration::from_millis(200)).ok().filter(|e| matches!(e, AppEvent::Worktree(_)))).expect("the git call finished");
        app.on_event(ev);
    };

    run_cmd(&mut app, "/worktree add feat/x");
    next(&mut app);
    assert_eq!(app.sessions.len(), 2, "a tab opened in the new folder");
    let wt = repo.parent().unwrap().join("app-feat-x");
    assert_eq!(app.cwd(), std::fs::canonicalize(&wt).unwrap().to_string_lossy());
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("worktree feat/x added")), "{:?}", app.flash);

    app.goto(0);
    run_cmd(&mut app, "/wt");
    let rows = screen(&mut app, 120, 24);
    assert!(rows.iter().any(|r| r.starts_with(" Worktrees")) && rows.iter().any(|r| r.contains("feat/x") && r.contains("1 tab")) && rows.iter().any(|r| r.contains("main") && r.contains("this tab")), "{}", rows.join("\n"));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('d'));
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("a tab is working in it")) && wt.exists(), "not while a tab is in it");

    app.sessions.remove(1);
    app.active = 0;
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('d'));
    next(&mut app);
    assert!(!wt.exists() && app.wt_ui.as_ref().unwrap().list.len() == 1, "removed, and the panel re-read the list");
    press(&mut app, KeyCode::Char('d'));
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("main worktree")));
    press(&mut app, KeyCode::Char('n'));
    typed(&mut app, "bad name");
    press(&mut app, KeyCode::Enter);
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("not a usable name")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn swarm_sends_one_message_asking_claude_to_use_parallel_subagents_and_needs_the_agent_tool() {
    let (mut app, _d) = fake_app("swarm");
    app.sessions[0].profile = "lean".into();
    run_cmd(&mut app, "/swarm 4 port the parser");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("no Agent tool")) && app.sessions[0].items.is_empty(), "nothing is sent to a tab without Agent");
    app.sessions[0].profile = "full".into();
    run_cmd(&mut app, "/swarm");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("usage")));
    run_cmd(&mut app, "/swarm 4 port the parser");
    let said: Vec<String> = app.sessions[0].items.iter().filter_map(|i| match &i.entry { session::Entry::User(t) => Some(t.clone()), session::Entry::Note(t) => Some(format!("NOTE {t}")), _ => None }).collect();
    assert!(said[0].starts_with("NOTE swarm: up to 4 subagents inside this one claude process"), "{said:?}");
    assert!(said[1].contains("at most 4 parts") && said[1].contains("Goal: port the parser") && !said[1].starts_with('/'), "{said:?}");
    assert!(app.sessions[0].busy, "and it went out like any message");
    assert_eq!(app.history.last().map(String::as_str), Some("/swarm 4 port the parser"), "history keeps what you typed");
    // a tab that has reported its tools: Agent decides
    app.sessions[0].busy = false;
    app.sessions[0].tools = vec!["Bash".into(), "Read".into()];
    run_cmd(&mut app, "/swarm do a thing");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("no Agent tool")));
}

#[test]
fn the_agent_terminal_is_offered_only_when_on_and_slash_term_drives_a_real_shell() {
    let (mut app, _d) = fake_app("term");
    assert!(app.launch_for(0).terms.is_none(), "off by default: no tools, no tokens");
    app.apply_setting("agent_term", "on");
    assert!(app.launch_for(0).terms.is_some());
    if crate::pane::open_pty(std::process::Command::new("true"), (2, 2)).is_err() {
        return eprintln!("skipped: no pty here");
    }
    run_cmd(&mut app, "/term");
    let t = app.term_view.clone().expect("a shell opened and has the screen");
    t.wait(0, None, Duration::from_secs(3));
    typed(&mut app, "echo ok-$((2+3))");
    press(&mut app, KeyCode::Enter);
    assert!(t.wait(0, Some("ok-5"), Duration::from_secs(5)), "keys reach the shell: {}", t.text(40));
    let rows = screen(&mut app, 100, 20);
    assert!(rows.iter().any(|r| r.contains("ok-5")) && rows.last().unwrap().contains("private") && rows.last().unwrap().contains("ctrl+\\ back"), "{rows:#?}");
    // ctrl+\ leaves; the shell keeps running and is in the list
    app.on_key(KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL));
    assert!(app.term_view.is_none() && t.exit().is_none());
    run_cmd(&mut app, "/term");
    assert!(app.term_ui.is_some() && app.term_view.is_none(), "with one running, /term lists");
    let rows = screen(&mut app, 100, 20).join("\n");
    assert!(rows.contains("yours, private") && rows.contains("running"), "{rows}");
    // the agent cannot see it until shared
    let terms = app.sessions[0].terms.clone();
    let agent = |terms: &std::sync::Arc<term::Terms>| {
        let (tx, rx) = mpsc::channel();
        terms.handle(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "term_list", "arguments": {}}}), Box::new(move |v| tx.send(v).unwrap()));
        rx.recv_timeout(Duration::from_secs(5)).unwrap()["result"]["content"][0]["text"].as_str().unwrap().to_string()
    };
    assert_eq!(agent(&terms), "no terminals");
    press(&mut app, KeyCode::Char('s'));
    assert!(agent(&terms).starts_with("you1:"), "{}", agent(&terms));
    press(&mut app, KeyCode::Char('x'));
    assert_eq!(terms.all().len(), 1, "one x only arms it");
    press(&mut app, KeyCode::Char('x'));
    assert!(terms.all().is_empty() && app.term_ui.is_some());
    press(&mut app, KeyCode::Esc);
    assert!(app.term_ui.is_none());
}


/// Every `draw` must reach the terminal as ONE synchronized update, and the cursor must end each one where the
/// input box wants it (or hidden while something else has the keyboard). ratatui's backend flushes after each
/// cursor command, which used to split a frame in two: cells, then the cursor shown where the last cell ended.
#[test]
fn each_frame_is_one_update_and_the_cursor_only_ever_rests_in_the_input() {
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(b); Ok(b.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let sink = Sink(Arc::new(Mutex::new(vec![])));
    let (mut app, _d) = fake_app("cursortrace");
    let frames = crate::screen::Frames::new(sink.clone());
    let mut term = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(frames.clone())).unwrap();
    term.resize(ratatui::layout::Rect::new(0, 0, 100, 30)).unwrap();
    let mut parser = vt100::Parser::new(std::num::NonZero::new(30).unwrap(), std::num::NonZero::new(100).unwrap(), 0);
    app.input = "hello".into();
    app.cursor = 5;
    let mut at = None;
    for step in 0..40 {
        if step == 5 { app.s().busy = true; app.s().busy_since = std::time::Instant::now(); }
        if step == 15 { app.s().note(format!("some output line {step}")); }
        if step == 30 { app.dock_focus = true; }
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        frames.present().unwrap();
        let bytes: Vec<u8> = std::mem::take(&mut *sink.0.lock().unwrap());
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.matches("\x1b[?2026h").count() <= 1, "step {step}: more than one update in a frame: {text:?}");
        assert_eq!(text.matches("\x1b[?2026h").count(), text.matches("\x1b[?2026l").count(), "step {step}: update left open");
        parser.process(&bytes);
        let s = parser.screen();
        if step == 30 {
            assert!(s.hide_cursor(), "the side list has the keyboard: no cursor in the input box");
        } else if step < 30 {
            assert!(!s.hide_cursor(), "step {step}: cursor visible");
            // ...and where the text ends, whatever was drawn above it
            let here = s.cursor_position();
            assert_eq!(here.1 as usize, 2 + 5, "step {step}: cursor column after `hello`: {here:?}");
            at.get_or_insert(here.0);
            assert_eq!(Some(here.0), at, "step {step}: the cursor row moved as the screen changed");
        }
    }
}

/// Work shows as a spinner, the turn's clock and, while tasks are open, how many are done as a bar.
#[test]
fn a_working_tab_shows_the_spinner_and_how_far_its_tasks_are() {
    let (mut app, _d) = fake_app("working-look");
    let s = &mut app.sessions[0];
    s.busy = true;
    s.activity = "calling Bash".into();
    s.tasks = ["completed", "in_progress", "pending"].iter().map(|st| ("1".into(), "t".into(), st.to_string())).collect();
    let rows = screen(&mut app, 100, 24);
    let line = rows.iter().find(|r| r.contains("Calling Bash…")).expect("the spinner line");
    assert!(line.contains("1/3") && line.contains("█") && line.contains("esc to interrupt"), "{line}");
}

#[test]
fn btw_asks_mid_turn_without_touching_the_turn_the_input_or_the_conversation() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-btw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    let d = dir.display();
    std::fs::write(&script, format!("#!/bin/sh\ncase \"$*\" in *--fork-session*) echo \"$@\" > {d}/args; cat > {d}/question; echo '{{\"result\":\"because of **X**\",\"total_cost_usd\":0.0042,\"is_error\":false}}';; *) cat > /dev/null;; esac\n")).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(Launch { bin: script.to_string_lossy().into(), ..Default::default() }, dir.to_string_lossy().into(), tx);
    app.new_tab();
    app.s().session_id = Some("sid-1".into());
    app.s().busy = true; // Claude is mid-turn
    app.queue_on = true; // and ordinary messages would wait in line
    let sid = app.s().id;
    let items = app.s().items.len();

    run_cmd(&mut app, "/btw");
    assert_eq!(app.s().items.len(), items, "a question is required");
    run_cmd(&mut app, "/btw why is it so?");
    assert!(app.s().busy && app.s().queue.is_empty(), "the turn is untouched and the question did not queue");
    assert!(matches!(&app.s().items.last().unwrap().entry, session::Entry::Side { answer: None, .. }), "shown at once, waiting");
    typed(&mut app, "next thing");
    assert_eq!(app.input, "next thing", "no panel took the keyboard");

    let ev = (0..100).find_map(|_| match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(e @ AppEvent::Btw(..)) => Some(e),
        _ => None,
    });
    app.on_event(ev.expect("the answer arrived"));
    assert!(matches!(&app.s().items.last().unwrap().entry, session::Entry::Side { answer: Some(Ok((t, _))), .. } if t.contains("because of")));
    assert!(app.s().busy, "still the same turn");

    let args = std::fs::read_to_string(dir.join("args")).unwrap();
    assert!(args.contains("--resume sid-1 --fork-session") && args.contains("--no-session-persistence"), "{args}");
    assert_eq!(std::fs::read_to_string(dir.join("question")).unwrap(), "why is it so?");

    // it is on screen, and an answer for a cleared conversation is dropped
    let lines = ui::entry_lines(&app.s().items.last().unwrap().entry, 60, false, "/");
    let text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(text.contains("btw") && text.contains("why is it so?") && text.contains("because of") && text.contains("$0.0042"), "{text}");
    app.on_event(AppEvent::Btw(sid, 99, Ok(("late".into(), 0.0))));
    app.on_event(AppEvent::Btw(sid + 1000, 1, Ok(("elsewhere".into(), 0.0))));
}

#[test]
fn procs_shows_what_is_read_groups_it_by_owner_and_closes() {
    let (mut app, _d) = fake_app("procs");
    run_cmd(&mut app, "/procs");
    typed(&mut app, "zz");
    assert!(app.input.is_empty(), "keys belong to the panel");
    // Until the (off-thread) read reports, the panel says so; the real read cannot be relied on in a test.
    app.procs_ui.as_mut().unwrap().set(None);
    assert!(screen(&mut app, 100, 30).join("\n").contains("reading the process list"));
    let row = |pid, kind, mb, cmd: &str| procs::Row { pid, kind, mb, cmd: cmd.into() };
    let report = procs::Report {
        rows: vec![row(10, procs::Kind::Ours, 190.0, "claude -p --input-format stream-json"), row(20, procs::Kind::Spare, 265.0, "claude bg-spare --bg-spare x")],
        source: "footprint",
    };
    app.procs_ui.as_mut().unwrap().set(Some(report));
    let text = screen(&mut app, 100, 30).join("\n");
    assert!(text.contains("ours (this claude++)") && text.contains("daemon spare workers") && text.contains("455 MB") && text.contains("bg-spare") && text.contains("only \"ours\" is controlled"), "{text}");
    press(&mut app, KeyCode::Char('r'));
    assert!(app.procs_ui.as_ref().unwrap().report.is_none(), "r reads the list again");
    press(&mut app, KeyCode::Esc);
    assert!(app.procs_ui.is_none() && !app.overlay_open());
}

fn three_tabs_with_backends(tag: &str) -> (App, std::path::PathBuf) {
    let (mut app, dir) = fake_app(tag);
    app.new_tab();
    app.new_tab(); // tabs 0,1,2; focus on 2
    for s in &mut app.sessions {
        s.session_id = Some("sid".into());
        s.backend = Some(Backend::spawn(&app.launch, Resume::New, app.tx.clone(), |_, m| AppEvent::Backend(0, 0, m)).unwrap());
        s.mem_kb = 100 * 1024;
    }
    (app, dir)
}

#[test]
fn the_tab_in_front_goes_only_when_the_supervisor_found_the_window_left_and_nothing_here_says_otherwise() {
    let (mut app, _d) = three_tabs_with_backends("sup-front");
    let front = app.active;
    let tab = app.sessions[front].id;
    let ask = |app: &mut App, front: bool| app.on_event(AppEvent::Sup(supervisor::Msg::Suspend { tab, why: "the tab was idle 600 s".into(), front, limit_s: 120 }));
    app.last_input = Instant::now() - Duration::from_secs(300);
    app.sessions[front].last_active = Instant::now() - Duration::from_secs(600);
    ask(&mut app, false);
    assert!(app.sessions[front].backend.is_some(), "a plain request never stops the tab in front");
    app.on_event(AppEvent::Term(Event::FocusGained));
    ask(&mut app, true);
    assert!(app.sessions[front].backend.is_some(), "the terminal has focus and you were here within five times the idle time: this is where you are");
    app.on_event(AppEvent::Term(Event::FocusLost));
    app.last_input = Instant::now();
    ask(&mut app, true);
    assert!(app.sessions[front].backend.is_some(), "you typed a moment ago");
    app.last_input = Instant::now() - Duration::from_secs(600);
    app.sessions[front].last_active = Instant::now();
    ask(&mut app, true);
    assert!(app.sessions[front].backend.is_some(), "the tab did something a moment ago: the report was stale");
    app.sessions[front].last_active = Instant::now() - Duration::from_secs(600);
    ask(&mut app, true);
    assert!(app.sessions[front].backend.is_none(), "left, quiet, and the supervisor says so: it goes");
}

#[test]
fn a_key_is_evidence_of_focus_and_a_terminal_that_never_reports_it_stays_unknown() {
    let (mut app, _d) = fake_app("unused");
    app.on_event(AppEvent::Term(Event::FocusLost));
    app.last_input = Instant::now() - Duration::from_secs(500);
    app.on_event(AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))));
    assert_eq!(app.focus, Some(true), "a key that reaches this window is evidence that the terminal has focus");
    app.on_event(AppEvent::Term(Event::Paste("x".into())));
    app.focus = None;
    app.on_event(AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))));
    assert_eq!(app.focus, None, "a terminal that never reports focus stays unknown");
}

#[test]
fn the_window_reports_what_each_tab_may_lose_and_the_supervisor_reports_what_each_holds() {
    let (mut app, _d) = three_tabs_with_backends("sup-report");
    app.sessions[0].busy = true;
    app.sessions[1].last_active = Instant::now() - Duration::from_secs(100);
    let tabs = app.tab_report();
    let by = |i: usize| tabs[i].clone();
    assert_eq!((by(0)["safe"].as_bool(), by(1)["safe"].as_bool(), by(2)["safe"].as_bool()), (Some(false), Some(true), Some(false)), "busy: no; idle in the background: yes; in front: not as an ordinary tab");
    assert_eq!(by(2)["safe_front"], true, "the tab in front may go when the window is found to be away");
    assert_eq!(by(1)["idle_s"], 90, "idle time in 30 s steps");
    assert!(by(0).get("mb").is_none(), "what a tab holds is measured by the supervisor, not said by the window");

    // The numbers come back with the peers, for this window's processes.
    let me = std::process::id();
    let pid = |i: usize| app.sessions[i].backend.as_ref().unwrap().pid;
    let tab = |i: usize, mb| supervisor::TabInfo { id: app.sessions[i].id, pid: Some(pid(i)), busy: false, mb, cwd: String::new() };
    let peers = vec![
        supervisor::Peer { pid: me, ui_mb: 12, tabs: vec![tab(0, 300), tab(2, 150)] },
        supervisor::Peer { pid: me + 1, ui_mb: 99, tabs: vec![tab(1, 9999)] },
    ];
    app.on_event(AppEvent::Sup(supervisor::Msg::Peers(peers)));
    assert_eq!((app.sessions[0].mem_kb, app.sessions[1].mem_kb, app.sessions[2].mem_kb, app.ui_mem_kb), (300 * 1024, 0, 150 * 1024, 12 * 1024));
}

#[test]
fn a_notice_from_the_supervisor_is_said_in_the_window() {
    let (mut app, _d) = fake_app("unused");
    app.on_event(AppEvent::Sup(supervisor::Msg::Notice("over the memory budget".into())));
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("over the memory budget")));
}

#[test]
fn scrolling_to_read_counts_as_using_the_window() {
    let (mut app, _d) = fake_app("unused");
    app.last_input = Instant::now() - Duration::from_secs(600);
    app.on_event(AppEvent::Term(Event::Mouse(ratatui::crossterm::event::MouseEvent {
        kind: ratatui::crossterm::event::MouseEventKind::ScrollDown,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    })));
    assert!(app.last_input.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_supervisor_picks_the_tab_and_this_window_only_refuses_one_it_cannot_stop() {
    let (mut app, _d) = three_tabs_with_backends("sup-suspend");
    let ask = |app: &mut App, i: usize| {
        let tab = app.sessions[i].id;
        app.on_event(AppEvent::Sup(supervisor::Msg::Suspend { tab, why: "all claude++ windows hold 900 MB, budget 500 MB".into(), front: false, limit_s: 120 }));
    };
    ask(&mut app, 2);
    assert!(app.sessions[2].backend.is_some(), "the tab in front is never stopped");
    app.sessions[0].busy = true;
    ask(&mut app, 0);
    assert!(app.sessions[0].backend.is_some(), "a tab that started working since is refused");
    ask(&mut app, 1);
    assert!(app.sessions[1].backend.is_none(), "an idle tab in the background goes");
    app.on_event(AppEvent::Sup(supervisor::Msg::Suspend { tab: 424242, why: String::new(), front: false, limit_s: 120 })); // a tab closed meanwhile
}

/// Seconds since the epoch, `ahead` from now: when a plan window resets.
fn reset_in(ahead: u64) -> f64 {
    (SystemTime::now() + Duration::from_secs(ahead)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64()
}

#[test]
fn a_plan_window_warns_once_when_it_fills_and_again_only_after_it_resets() {
    let (mut app, _d) = fake_app("limit-warn");
    app.sessions[0].five_hour = Some((85.0, reset_in(3600)));
    app.warn_on_limits();
    assert!(app.flash.is_none(), "90% is the default level: 85% is quiet");
    app.sessions[0].five_hour = Some((92.0, reset_in(3600)));
    app.warn_on_limits();
    let notes = |app: &mut App| app.s().items.iter().filter(|i| i.entry.text().contains("5-hour limit at 92%")).count();
    assert_eq!(notes(&mut app), 1);
    assert!(app.flash.is_some());
    app.warn_on_limits();
    app.sessions[0].five_hour = Some((96.0, app.sessions[0].five_hour.unwrap().1));
    app.warn_on_limits();
    assert_eq!(notes(&mut app), 1, "the same window does not warn again as it climbs");
    app.sessions[0].five_hour = Some((91.0, reset_in(5 * 3600)));
    app.warn_on_limits();
    assert_eq!(app.s().items.iter().filter(|i| i.entry.text().contains("5-hour limit at")).count(), 2, "a new window warns again");
    app.apply_setting("limit_warn", "0");
    app.limit_warned.clear();
    app.flash = None;
    app.warn_on_limits();
    assert!(app.flash.is_none(), "off is off");
}

#[test]
fn a_message_over_the_hold_level_waits_for_the_reset_and_send_now_goes_ahead() {
    let (mut app, _d) = fake_app("limit-hold");
    let cfg = std::env::temp_dir().join(format!("claudeplusplus-cfg-limit-{}", std::process::id())).join("config.json");
    let _ = std::fs::remove_file(&cfg);
    app.config_path = Some(cfg.clone());
    app.sessions[0].seven_day = Some((97.0, reset_in(7200)));
    typed(&mut app, "before");
    press(&mut app, KeyCode::Enter);
    assert_eq!(sent(&mut app), ["before"], "the hold is off until chosen");
    app.s().busy = false;

    app.apply_setting("limit_hold", "95");
    assert_eq!(config::get_str(&cfg, "limit_hold").as_deref(), Some("95"));
    typed(&mut app, "held");
    press(&mut app, KeyCode::Enter);
    assert_eq!(sent(&mut app), ["before"]);
    assert_eq!(queued(&mut app), ["held"]);
    let due = app.s().queue[0].due.expect("due at the reset").duration_since(SystemTime::now()).unwrap().as_secs();
    assert!((7190..=7200).contains(&due), "{due}");
    assert!(app.s().items.iter().any(|i| i.entry.text().contains("7-day limit at 97%") && i.entry.text().contains("held")));
    app.drain_queues();
    assert_eq!(sent(&mut app), ["before"], "the clock has not come");

    run_cmd(&mut app, "/help");
    assert!(app.s().queue.len() == 1 && app.s().items.iter().any(|i| i.entry.text().contains("KEYS")), "a slash command is not held");

    // A message that waited for a turn also waits for the window.
    app.s().queue.push(session::Queued { text: "behind".into(), atts: vec![], due: None });
    app.drain_queues();
    assert!(app.s().queue.iter().all(|q| q.due.is_some()) && sent(&mut app) == ["before"]);

    // Send now: this window no longer holds, the next one will.
    alt(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Esc);
    app.s().busy = false;
    app.drain_queues();
    assert_eq!(sent(&mut app), ["before", "held"]);
    app.s().busy = false;
    typed(&mut app, "after");
    press(&mut app, KeyCode::Enter);
    assert_eq!(sent(&mut app), ["before", "held", "after"], "released for the rest of this window");
    app.sessions[0].seven_day = Some((98.0, reset_in(9000)));
    app.s().busy = false;
    typed(&mut app, "next window");
    press(&mut app, KeyCode::Enter);
    assert!(queued(&mut app).contains(&"next window".to_string()), "a different window holds again");
}

#[test]
fn rate_limits_are_a_config_tab_and_a_window_only_counts_until_it_resets() {
    let (mut app, _d) = fake_app("limit-cfg");
    app.sessions[0].five_hour = Some((99.0, reset_in(60)));
    app.apply_setting("limit_hold", "99");
    assert!(app.limit_block().is_some());
    app.sessions[0].five_hour = Some((99.0, reset_in(0).min(1.0)));
    assert!(app.limit_block().is_none(), "a window past its reset holds nothing");
    assert_eq!(app.setting_value("limit_hold"), "99");
    assert_eq!(app.setting_value("limit_warn"), "90");
    run_cmd(&mut app, "/config");
    press(&mut app, KeyCode::Tab); // the section strip has the keys
    for _ in 0..4 {
        press(&mut app, KeyCode::Right);
    }
    let text = screen(&mut app, 110, 30).join("\n");
    assert!(text.contains("Rate limits") && text.contains("Warn at") && text.contains("Hold messages at"), "{text}");
}

fn sup_dir(tag: &str) -> supervisor::Paths {
    let dir = std::env::temp_dir().join(format!("cpp-supui-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    supervisor::Paths { dir }
}

#[test]
fn supervisor_panel_without_a_supervisor_edits_the_saved_settings_and_starts_it() {
    let (mut app, _d) = fake_app("supui-down");
    app.sup_paths = sup_dir("down");
    let cfg = std::env::temp_dir().join(format!("claudeplusplus-cfg-supui-{}", std::process::id())).join("config.json");
    let _ = std::fs::remove_file(&cfg);
    app.config_path = Some(cfg.clone());
    run_cmd(&mut app, "/supervisor");
    assert!(app.sup_ui.is_some());
    let text = screen(&mut app, 110, 30).join("\n");
    assert!(text.contains(" Supervisor") && text.contains("NOT RUNNING") && text.contains("Stays after the last window") && text.contains("Start"), "{text}");
    assert!(!text.contains("use the panel above"), "a panel, not the message box\n{text}");
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Right); // Clean up after a crash: on -> off
    assert_eq!(config::get_str(&cfg, "supervisor_cleanup").as_deref(), Some("off"));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Left); // Memory budget: enforced -> paused (wraps)
    assert_eq!(config::get_str(&cfg, "supervisor_budget").as_deref(), Some("paused"));
    assert!(supervisor::Config::user().enforce, "the user's own config is not this test's");
    // Start: allowed again, and the next connect attempt is due at once.
    supervisor::stop_on_purpose(&app.sup_paths).unwrap();
    assert!(screen(&mut app, 110, 30).join("\n").contains("STOPPED") && screen(&mut app, 110, 30).join("\n").contains("nothing starts it"));
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert!(!app.sup_paths.stopped_marker().exists() && app.sup_tried.elapsed() > Duration::from_secs(30));
    press(&mut app, KeyCode::Esc);
    assert!(app.sup_ui.is_none());
}

#[test]
fn supervisor_panel_shows_the_live_state_and_manages_it() {
    // The supervisor lists the shared MCP servers of the whole process, and each is a row in the panel: another test's
    // servers would move the rows this one navigates by. (One supervisor per process in real life.)
    let _own = crate::sync::LockOk::lock_ok(&share::tests::HUB_TEST);
    let (mut app, _d) = fake_app("supui-live");
    app.sup_paths = sup_dir("live");
    let cfg = std::env::temp_dir().join(format!("claudeplusplus-cfg-supui-live-{}", std::process::id())).join("config.json");
    let _ = std::fs::remove_file(&cfg);
    app.config_path = Some(cfg.clone());
    let (p, done) = (app.sup_paths.clone(), std::sync::mpsc::channel::<()>());
    std::thread::spawn(move || {
        supervisor::run(&p, &supervisor::Config { grace: Duration::from_secs(60), ..Default::default() }).unwrap();
        let _ = done.0.send(());
    });
    let t = Instant::now();
    while !app.sup_paths.sock().exists() {
        assert!(t.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    let (tx, rx) = mpsc::channel();
    let client = supervisor::Client::connect(&app.sup_paths, std::process::id(), tx, |m| m).expect("connect");
    app.sup = Some(Arc::new(client));
    // What the supervisor says arrives as events; pump them until the panel has its status.
    let pump = |app: &mut App| {
        while let Ok(m) = rx.try_recv() {
            app.on_event(AppEvent::Sup(m));
        }
    };
    run_cmd(&mut app, "/supervisor");
    let t = Instant::now();
    while app.sup_ui.as_ref().unwrap().status.is_none() {
        assert!(t.elapsed() < Duration::from_secs(5), "no status");
        std::thread::sleep(Duration::from_millis(20));
        pump(&mut app);
    }
    let text = screen(&mut app, 110, 34).join("\n");
    assert!(text.contains("RUNNING") && text.contains(&format!("pid {}", std::process::id())) && text.contains("Windows") && text.contains("(this one)") && text.contains("no budget set"), "{text}");
    assert!(text.contains("Restart") && text.contains("Stop") && text.contains("Activity") && text.contains("connected") && text.contains("Memory") && text.contains("since it started"), "{text}");

    // A setting is changed in the supervisor itself and saved.
    press(&mut app, KeyCode::Right); // Stays after the last window: 60 s is none of the choices, so → first
    let t = Instant::now();
    loop {
        pump(&mut app);
        if config::get_str(&cfg, "supervisor_grace_s").is_some() && app.sup_setting(0) == "20" {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "the supervisor never answered the change: {}", app.sup_setting(0));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(config::get_str(&cfg, "supervisor_grace_s").as_deref(), Some("20"));

    // Stop: it leaves and stays stopped.
    for _ in 0..3 {
        press(&mut app, KeyCode::Down); // settings 1, 2, then the buttons
    }
    press(&mut app, KeyCode::Right); // Restart -> Stop
    press(&mut app, KeyCode::Enter);
    done.1.recv_timeout(Duration::from_secs(5)).expect("the supervisor left");
    assert!(app.sup_paths.stopped_marker().exists());
    assert!(app.flash.as_ref().is_some_and(|f| f.to_string().contains("stopped")));
}

#[test]
fn a_hint_names_the_message_being_answered_only_when_it_is_off_screen() {
    use session::Entry;
    let (mut app, _dir) = fake_app("hint");
    let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 14)).unwrap();
    let mut count = |app: &mut App| {
        t.draw(|f| ui::draw(f, app)).unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect::<String>().matches("fix the zebra bug").count()
    };
    app.sessions[0].push(Entry::User("fix the zebra bug\nin detail please".into()));
    app.sessions[0].push(Entry::Note("first".into()));
    assert_eq!(count(&mut app), 1, "the message is on screen: no hint");
    for i in 0..40 {
        app.sessions[0].push(Entry::Note(format!("line {i}")));
    }
    assert_eq!(count(&mut app), 1, "the answer pushed it off the top: the hint is the only place it shows");
    app.sessions[0].scroll = 10_000;
    assert_eq!(count(&mut app), 1, "scrolled back to it: it shows itself, not twice");
}

#[test]
fn the_end_of_turn_note_says_when_it_stopped() {
    let (mut app, _dir) = fake_app("done");
    let s = &mut app.sessions[0];
    s.handle(&serde_json::from_str(r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi there"}}}"#).unwrap());
    s.handle(&serde_json::from_str(r#"{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#).unwrap());
    s.handle(&serde_json::from_str(r#"{"type":"result","subtype":"success","is_error":false,"result":"hi"}"#).unwrap());
    let note = s.items.iter().rev().find_map(|i| if let session::Entry::Note(t) = &i.entry { Some(t.clone()) } else { None }).unwrap_or_default();
    let re = note.strip_prefix("✻ Worked for ").and_then(|r| r.split_once(" · done ")).map(|(_, c)| c.len() == 5 && c.as_bytes()[2] == b':');
    assert_eq!(re, Some(true), "{note:?}");
}

#[test]
fn a_conversation_selection_copies_while_dragging_stays_on_its_words_as_output_arrives_and_scrolls_at_the_edge() {
    use ratatui::crossterm::event::{MouseButton::Left, MouseEventKind::{Down, Drag, Up}};
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, _d) = fake_app("selfollow");
    for i in 0..30 {
        app.s().push(session::Entry::Note(format!("earlier {i}")));
    }
    app.s().push(session::Entry::Assistant { text: "alpha beta gamma".into(), msg_id: String::new(), streaming: false });
    let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
    let mut draw = |app: &mut App| {
        term.draw(|f| ui::draw(f, app)).unwrap();
        let b = term.backend().buffer().clone();
        let rows: Vec<String> = (0..24).map(|y| (0..100).map(|x| b[(x, y)].symbol().to_string()).collect()).collect();
        let marked: Vec<(u16, u16)> = (0..24).flat_map(|y| (0..100).map(move |x| (x, y))).filter(|&(x, y)| b[(x, y)].bg == theme::selection().bg.unwrap()).collect();
        (rows, marked)
    };
    let (rows, _) = draw(&mut app);
    let (y, x) = rows.iter().enumerate().find_map(|(y, r)| r.find("alpha").map(|i| (y as u16, r[..i].chars().count() as u16))).unwrap();
    mouse_ev(&mut app, Down(Left), x, y);
    mouse_ev(&mut app, Drag(Left), x + 9, y);
    draw(&mut app);
    assert!(app.sent_notices.contains(&"copy:alpha beta".to_string()), "copied before the button is up: {:?}", app.sent_notices);
    mouse_ev(&mut app, Up(Left), x + 9, y);
    // output arrives below: the answer moves up the screen and the highlight moves with it
    for i in 0..3 {
        app.s().push(session::Entry::Note(format!("later {i}")));
    }
    let (rows, marked) = draw(&mut app);
    let (y2, _) = rows.iter().enumerate().find_map(|(y, r)| r.find("alpha").map(|i| (y as u16, i))).unwrap();
    assert!(y2 < y, "the answer moved up");
    assert_eq!(marked.len(), 10, "ten cells stay marked");
    assert!(marked.iter().all(|&(mx, my)| my == y2 && (x..x + 10).contains(&mx)), "{marked:?} on row {y2}");
    // dragging above the conversation scrolls it and the end follows
    for i in 0..60 {
        app.s().push(session::Entry::Note(format!("filler {i}")));
    }
    draw(&mut app);
    let (area, before) = (app.sessions[0].sel.area, app.sessions[0].scroll);
    mouse_ev(&mut app, Down(Left), area.x + 5, area.bottom() - 2);
    mouse_ev(&mut app, Drag(Left), area.x + 5, 0);
    std::thread::sleep(Duration::from_millis(60));
    app.autoscroll_selection();
    assert_eq!(app.sessions[0].scroll, before + 1, "a pointer above the conversation scrolls it");
    mouse_ev(&mut app, Up(Left), area.x + 5, 0);
}

#[test]
fn shift_arrows_select_in_the_message_box_copy_as_they_go_and_typing_replaces_the_selection() {
    let (mut app, _d) = fake_app("boxsel");
    let key = |app: &mut App, c, m| app.on_key(KeyEvent::new(c, m));
    typed(&mut app, "hello big world");
    for _ in 0..5 {
        key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    }
    assert_eq!(app.box_selection(), Some((10, 15)));
    screen(&mut app, 80, 24); // the frame after each key puts the selection on the clipboard
    assert_eq!(app.sent_notices.last().map(String::as_str), Some("copy:world"), "copied while selecting");
    key(&mut app, KeyCode::Left, KeyModifiers::SHIFT | KeyModifiers::ALT);
    assert_eq!(app.box_selection(), Some((6, 15)), "alt+shift takes a word at a time");
    screen(&mut app, 80, 24);
    assert_eq!(app.sent_notices.last().map(String::as_str), Some("copy:big world"));
    key(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert!(app.box_selection().is_none(), "a plain arrow drops it");
    key(&mut app, KeyCode::Home, KeyModifiers::SHIFT);
    key(&mut app, KeyCode::Char('X'), KeyModifiers::SHIFT);
    assert_eq!(app.input, "Xig world", "typing replaces the selection (the cursor was after \"hello b\")");
    typed(&mut app, "yz");
    key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.input, "Xyig world", "backspace deletes the selection, not one more");
    key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert_eq!(app.input, "Xyzig world", "and ctrl+z brings it back in one step");
}

#[test]
fn a_click_in_the_message_box_moves_the_cursor_and_a_drag_selects_and_copies_live() {
    use ratatui::crossterm::event::{MouseButton::Left, MouseEventKind::{Down, Drag, Up}};
    let (mut app, _d) = fake_app("boxclick");
    typed(&mut app, "hello big world\nsecond line here");
    let rows = screen(&mut app, 80, 24);
    let (y, x0) = rows.iter().enumerate().find_map(|(y, r)| r.find("hello").map(|i| (y as u16, r[..i].chars().count() as u16))).unwrap();
    mouse_ev(&mut app, Down(Left), x0 + 6, y);
    mouse_ev(&mut app, Up(Left), x0 + 6, y);
    assert_eq!((app.cursor, app.box_selection()), (6, None), "a click puts the cursor on the character");
    mouse_ev(&mut app, Down(Left), x0 + 40, y);
    mouse_ev(&mut app, Up(Left), x0 + 40, y);
    assert_eq!(app.cursor, 15, "past the end of a line: its end, before the newline");
    mouse_ev(&mut app, Down(Left), x0 + 6, y);
    mouse_ev(&mut app, Drag(Left), x0 + 9, y);
    screen(&mut app, 80, 24);
    assert_eq!(app.box_selection(), Some((6, 9)));
    assert!(app.sent_notices.contains(&"copy:big".to_string()), "copied while the button is down: {:?}", app.sent_notices);
    mouse_ev(&mut app, Up(Left), x0 + 9, y);
    screen(&mut app, 80, 24);
    assert_eq!(app.box_selection(), Some((6, 9)), "it stays after the button is up");
    assert!(app.flash.as_deref().is_some_and(|f| f.contains("copied 3 characters")), "{:?}", app.flash);
    // typing over it, as with shift+arrows: one selection model for the mouse and the keyboard
    app.on_event(AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT))));
    assert_eq!(app.input, "hello X world\nsecond line here");
}

#[test]
fn input_index_follows_the_wrapped_layout() {
    use ui::input_index as at;
    assert_eq!(at("hello", 10, 0, 3), 3);
    assert_eq!(at("hello", 10, 0, 99), 5, "past the end: after the last character");
    assert_eq!(at("ab\ncd", 10, 1, 1), 4);
    assert_eq!(at("ab\ncd", 10, 0, 7), 2, "past the end of a row: before its newline");
    assert_eq!(at("abcdef", 3, 1, 1), 4, "a wrapped row starts where the layout broke it");
    assert_eq!(at("abcdef", 3, 0, 9), 3, "the end of a wrapped row is where the next begins");
    assert_eq!(at("a\n\nb", 10, 1, 0), 2, "an empty row");
    assert_eq!(at("日本語", 4, 0, 3), 1, "the second cell of a wide character");
}

#[test]
fn drag_reporting_is_switched_on_after_plain_reporting_because_terminals_keep_only_the_last() {
    let on = keys::mouse_modes(true);
    let (plain, drag) = (on.find("\x1b[?1000h").unwrap(), on.find("\x1b[?1002h").unwrap());
    assert!(plain < drag, "1002 last, or Ghostty reports no drag: {on:?}");
    assert!(on.contains("\x1b[?1006h"));
    let off = keys::mouse_modes(false);
    assert!(off.contains("\x1b[?1002l") && off.contains("\x1b[?1000l") && off.contains("\x1b[?1006l"));
}

#[test]
fn a_terminal_emulator_ends_up_reporting_drags_and_is_left_clean() {
    // The bug this guards: with 1002 set before 1000 a terminal that keeps the last mode reported no drag at all.
    // vt100 keeps the last mode like such a terminal does.
    let mut t = vt100::Parser::new_with_callbacks(pane::nz(24), pane::nz(80), 0, pane::Replies::default());
    t.process(keys::mouse_modes(true).as_bytes());
    let s = t.screen();
    assert_eq!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::ButtonMotion, "drags are reported");
    assert_eq!(s.mouse_protocol_encoding(), vt100::MouseProtocolEncoding::Sgr);
    t.process(tty::RESET.as_bytes());
    let s = t.screen();
    assert_eq!(s.mouse_protocol_mode(), vt100::MouseProtocolMode::None, "nothing is reported after we leave");
    assert_eq!(s.mouse_protocol_encoding(), vt100::MouseProtocolEncoding::Default);
    assert!(tty::RESET.contains("\x1b[>0s"), "shift capture is handed back too");
    assert!(keys::mouse_modes(false).contains("\x1b[>0s"));
}
