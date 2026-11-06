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
    assert_eq!(s.items.len(), 3, "result text already shown is not duplicated");
    assert!(matches!(&s.items[2].entry, Entry::Note(n) if n.starts_with("✻ Worked for ")), "how long the turn took");
}

#[test]
fn an_error_said_as_a_reply_and_as_the_result_shows_once() {
    let mut s = Session::new(1, String::new());
    feed(
        &mut s,
        &[
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m","content":[{"type":"text","text":"There's an issue with the selected model."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"There's an issue with the selected model."}"#,
        ],
    );
    assert_eq!(s.items.len(), 1);
    assert!(matches!(&s.items[0].entry, Entry::Error(_)));
}

#[test]
fn a_rewind_finds_the_saved_prompt_even_when_the_same_words_were_sent_twice() {
    let saved = [
        r#"{"type":"user","uuid":"u1","parentUuid":"h","message":{"content":"run the tests"}}"#,
        r#"{"type":"user","uuid":"r1","parentUuid":"a1","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}}"#,
        r#"{"type":"user","uuid":"u2","parentUuid":"a2","message":{"content":"<bash-input>ls</bash-input><bash-stdout>x</bash-stdout><bash-stderr></bash-stderr>\n\nrun  the tests"}}"#,
        r#"{"type":"user","uuid":"u3","parentUuid":"a3","message":{"content":[{"type":"text","text":"what is"},{"type":"image","source":{}},{"type":"text","text":"this"}]}}"#,
    ]
    .map(|l| serde_json::from_str::<Value>(l).unwrap());
    let saved_prompt = |t: &str, later| saved_prompt(saved.to_vec(), t, later);
    assert_eq!(saved_prompt("run the tests", 0).unwrap()["uuid"], "u2", "the last one, typed after a !command");
    assert_eq!(saved_prompt("run the tests", 1).unwrap()["parentUuid"], "h");
    assert_eq!(saved_prompt("what is [Image #1] this", 0).unwrap()["uuid"], "u3");
    assert!(saved_prompt("never said", 0).is_none());
}

#[test]
fn a_hook_that_fails_is_mentioned_and_one_that_succeeds_is_not() {
    let mut s = Session::new(1, String::new());
    feed(
        &mut s,
        &[
            r#"{"type":"system","subtype":"hook_response","hook_name":"SessionStart:startup","outcome":"success","stderr":""}"#,
            r#"{"type":"system","subtype":"hook_response","hook_name":"PreToolUse:Bash","outcome":"error","exit_code":1,"stderr":"\nlint failed\nmore"}"#,
        ],
    );
    assert_eq!(s.items.len(), 1);
    assert!(matches!(&s.items[0].entry, Entry::Note(t) if t == "hook PreToolUse:Bash error: lint failed"));
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
fn a_resumed_agent_is_finished_by_its_saved_notification_not_shown_as_a_message() {
    let mut s = Session::new(1, String::new());
    let lines = [
        r#"{"type":"assistant","message":{"id":"m","content":[{"type":"tool_use","id":"a1","name":"Agent","input":{"description":"count"}}]}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a1","content":"Async agent launched successfully."}]}}"#,
        r#"{"type":"user","message":{"content":"<task-notification>\n<tool-use-id>a1</tool-use-id>\n<status>completed</status>\n<summary>Agent \"count\" finished</summary>\n</task-notification>"}}"#,
    ];
    for l in lines {
        s.replay(&serde_json::from_str(l).unwrap());
    }
    assert_eq!(s.items.len(), 1, "the notification is not a user message");
    assert!(matches!(&s.items[0].entry, Entry::Tool { result: Some((r, false)), .. } if r == "Agent \"count\" finished"));
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
fn only_what_the_current_backend_started_counts_as_background_work() {
    let mut s = Session::new(1, String::new());
    s.push(Entry::User("old question".into()));
    s.push(Entry::Tool { id: "b".into(), name: "Bash".into(), input: json!({"command": "sleep 99", "run_in_background": true}), result: None, nested: false });
    assert!(s.has_background_work(), "before any spawn everything on screen counts");
    s.bg_from = s.items.len(); // a backend spawns after history was replayed
    assert!(!s.has_background_work(), "replayed background work died with the old backend");
    s.push(Entry::Tool { id: "c".into(), name: "Bash".into(), input: json!({"run_in_background": true}), result: None, nested: false });
    assert!(s.has_background_work());
}

#[test]
fn a_permission_mode_chosen_is_remembered_for_the_next_backend() {
    let mut s = Session::new(1, String::new());
    s.control("set_permission_mode", json!({"mode": "plan"}));
    assert_eq!(s.mode_pref.as_deref(), Some("plan"));
}

#[test]
fn an_agent_backgrounded_by_default_blocks_suspension_until_its_report_arrives() {
    let mut s = Session::new(1, String::new());
    let agent = |r: &str| Entry::Tool { id: "g".into(), name: "Agent".into(), input: json!({"prompt": "p"}), result: Some((r.into(), false)), nested: false };
    s.push(agent(BACKGROUND));
    assert!(s.has_background_work());
    s.items.clear();
    s.push(agent("the report"));
    assert!(!s.has_background_work());
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
fn a_missing_conversation_is_not_resumed_again() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-gone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> {}/args\necho 'No conversation found with session ID: x' >&2\n", dir.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let launch = crate::backend::Launch { bin: script.to_string_lossy().into(), ..Default::default() };
    let (tx, rx) = std::sync::mpsc::channel();
    let mut s = Session::new(1, String::new());
    s.session_id = Some("x".into());
    s.warm(&launch, &tx);
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(5)).expect("backend exited") {
            AppEvent::Backend(_, pid, m @ Msg::Exited { .. }) => break s.on_backend(pid, m),
            AppEvent::Backend(_, pid, m) => s.on_backend(pid, m),
            _ => {}
        }
    }
    assert!(s.session_id.is_none() && matches!(s.start, Start::New));
    assert!(s.items.iter().any(|i| matches!(&i.entry, Entry::Error(e) if e.contains("new conversation"))));
    s.warm(&launch, &tx);
    drop(s);
    let args = (0..60)
        .find_map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::fs::read_to_string(dir.join("args")).ok().filter(|t| t.lines().count() >= 2)
        })
        .expect("respawned");
    assert!(args.lines().next().unwrap().contains("--resume x") && !args.lines().nth(1).unwrap().contains("--resume"));
}

#[test]
fn hostile_transcript_files_never_panic_resume_or_the_readers() {
    use crate::tests::{Rng, mutate, seed_messages};
    let seeds = seed_messages();
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let mut r = Rng(env("FUZZ_SEED", 0xDEAD_BEEF_CAFE_F00D) | 1);
    let dir = std::env::temp_dir().join(format!("claudeplusplus-hostile-transcripts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for n in 0..env("FUZZ_FILES", 25) {
        let mut bytes: Vec<u8> = vec![];
        for _ in 0..150 {
            match r.below(10) {
                0 => bytes.extend(b"{\"type\":\"user\",\"message\":{\"con"), // cut off mid-write
                1 => bytes.extend(b"null"),
                2 => bytes.extend(b"[1,2"),
                3 => bytes.extend([0xff, 0xfe, b'{', 0xc3]), // not UTF-8
                4 => bytes.extend(b"\"just a string\""),
                5 if r.below(40) == 0 => bytes.extend(vec![b'x'; 3_000_000]), // one enormous line
                _ => {
                    let mut m = seeds[r.below(seeds.len())].clone();
                    for _ in 0..r.below(3) {
                        mutate(&mut m, &mut r);
                    }
                    bytes.extend(m.to_string().into_bytes());
                }
            }
            bytes.push(b'\n');
        }
        let path = dir.join(format!("{n}.jsonl"));
        std::fs::write(&path, &bytes).unwrap();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut s = Session::new(1, String::new());
            s.show_history(&path);
            let _ = (history::preview(&path, 10), history::prompts(&path), history::session_name(&path), history::session_color(&path), history::agent_reports(&path));
        }));
        assert!(res.is_ok(), "transcript {} panicked resume or a reader (kept for inspection)", path.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn only_a_newer_major_minor_than_verified_is_flagged() {
    assert!(!newer_than_verified("2.1.317") && !newer_than_verified("2.1.0") && !newer_than_verified("2.0.9"));
    assert!(newer_than_verified("2.2.0") && newer_than_verified("3.0.1"));
    assert!(!newer_than_verified("") && !newer_than_verified("garbage"));
}

#[test]
fn typing_stops_restarting_a_backend_that_keeps_dying_at_start() {
    let dir = std::env::temp_dir().join(format!("claudeplusplus-dies-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    std::fs::write(&script, format!("#!/bin/sh\necho x >> {}/n\necho boom >&2\n", dir.display())).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&script).status().unwrap();
    let launch = crate::backend::Launch { bin: script.to_string_lossy().into(), ..Default::default() };
    let (tx, rx) = std::sync::mpsc::channel();
    let mut s = Session::new(1, String::new());
    for _ in 0..5 {
        s.warm(&launch, &tx); // one per keystroke
        if s.backend.is_some() {
            while let Ok(AppEvent::Backend(_, pid, m)) = rx.recv_timeout(std::time::Duration::from_secs(5)) {
                let done = matches!(m, Msg::Exited { .. });
                s.on_backend(pid, m);
                if done { break; }
            }
        }
    }
    assert_eq!(std::fs::read_to_string(dir.join("n")).unwrap().lines().count(), 2, "two tries, then it stops");
    s.submit("hi".into(), &[], &launch, &tx); // an explicit send still tries
    assert!(s.backend.is_some());
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
    s.push(Entry::Assistant { text: "Context Usage".into(), msg_id: "m".into(), streaming: false });
    feed(
        &mut s,
        &[
            r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"post_tokens":10912,"preserved_messages":{"uuids":["u1"]}}}"#,
            r#"{"type":"assistant","uuid":"u1","parent_tool_use_id":null,"message":{"id":"m2","content":[{"type":"text","text":"Context Usage"}]}}"#,
        ],
    );
    assert_eq!(s.items.iter().filter(|i| matches!(&i.entry, Entry::Assistant { text, .. } if text == "Context Usage")).count(), 1, "kept messages are not shown twice");
    assert_eq!(s.ctx_tokens, 0, "the old count is gone; the next call reports the new one");
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

#[test]
fn parallel_subagents_steps_stay_under_their_own_agent_while_text_streams() {
    let mut s = Session::new(1, String::new());
    let agent = |id: &str| format!(r#"{{"type":"assistant","parent_tool_use_id":null,"message":{{"id":"m{id}","content":[{{"type":"tool_use","id":"{id}","name":"Agent","input":{{}}}}]}}}}"#);
    let step = |parent: &str, id: &str| {
        format!(r#"{{"type":"assistant","parent_tool_use_id":"{parent}","message":{{"id":"n{id}","content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{}}}}]}}}}"#)
    };
    feed(&mut s, &[&agent("a"), &agent("b")]);
    feed(
        &mut s,
        &[
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
        ],
    );
    feed(&mut s, &[&step("b", "b1"), &step("a", "a1"), &step("b", "b2")]);
    feed(
        &mut s,
        &[r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#],
    );
    let ids: Vec<String> = s
        .items
        .iter()
        .map(|i| match &i.entry {
            Entry::Tool { id, .. } => id.clone(),
            Entry::Assistant { text, .. } => format!("text:{text}"),
            _ => "?".into(),
        })
        .collect();
    assert_eq!(ids, ["a", "a1", "b", "b1", "b2", "text:hi"]);
    // Agent a reports: its step folds into it, and text still streaming below keeps flowing.
    feed(
        &mut s,
        &[
            r#"{"type":"system","subtype":"task_notification","tool_use_id":"a","status":"completed","handback_report":{"text":"done"}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":null,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}}"#,
        ],
    );
    assert_eq!(s.items.len(), 5);
    assert!(matches!(&s.items[0].entry, Entry::Tool { input, result: Some((r, false)), .. } if r == "done" && input["_steps"][0]["name"] == "Bash"));
    assert!(matches!(&s.items[4].entry, Entry::Assistant { text, .. } if text == "hi!"));
}

#[test]
fn a_shell_command_finishing_after_clear_leaves_the_new_conversation_alone() {
    let mut s = Session::new(1, String::new());
    let old = s.start_shell("cat .env");
    s.clear();
    let new = s.start_shell("ls");
    assert_ne!(old, new);
    s.shell_done(&old, "cat .env", "SECRET=1", "", true);
    assert!(matches!(&s.items[0].entry, Entry::Tool { result: None, .. }), "the old result lands nowhere");
    s.shell_done(&new, "ls", "a.txt", "", true);
    let ctx = s.shell_context.concat();
    assert!(ctx.contains("a.txt") && !ctx.contains("SECRET"));
}

#[test]
fn shell_output_is_capped_in_total_per_command_and_across_commands() {
    let mut s = Session::new(1, String::new());
    let run = |s: &mut Session, cmd: &str, out: &str, err: &str| {
        let id = s.start_shell(cmd);
        s.shell_done(&id, cmd, out, err, true);
    };
    run(&mut s, "big", &"o".repeat(50_000), &"e".repeat(50_000));
    let ctx = s.shell_context.concat();
    assert!(ctx.chars().count() <= SHELL_CAP + 200, "output of 100k chars was cut to the cap plus tags: {}", ctx.chars().count());
    assert!(ctx.matches("ooooo").count() > 0 && ctx.matches("eeeee").count() > 0, "a chatty stdout must not starve the error text");
    for i in 0..5 {
        run(&mut s, &format!("c{i}"), &"x".repeat(20_000), "");
    }
    let total: usize = s.shell_context.iter().map(|b| b.chars().count()).sum();
    assert!(total <= SHELL_CAP + 200 && s.shell_context.last().unwrap().contains("c4"), "oldest dropped, newest kept: {total}");
}

#[test]
fn approving_a_plan_offers_auto_accept_and_the_mode_label_follows() {
    let ask = |tool: &str, sugg: &str| {
        format!(r#"{{"type":"control_request","request_id":"r","request":{{"subtype":"can_use_tool","tool_name":"{tool}","input":{{"plan":"p"}},"permission_suggestions":{sugg}}}}}"#)
    };
    let mut s = Session::new(1, String::new());
    s.mode = "plan".into();
    feed(&mut s, &[&ask("ExitPlanMode", "[]")]);
    assert_eq!(s.pending[0].suggestions[0]["mode"], "acceptEdits", "the same choice Claude Code offers");
    s.answer(Choice::Allow);
    assert_eq!(s.mode, "default", "plan mode is over once the plan is approved");
    assert_eq!(s.mode_pref.as_deref(), Some("default"), "a respawned backend must not go back to plan");
    s.mode = "plan".into();
    feed(&mut s, &[&ask("ExitPlanMode", "[]")]);
    s.answer(Choice::Always);
    assert_eq!(s.mode, "acceptEdits");
    feed(&mut s, &[&ask("Bash", r#"[{"type":"addRules","rules":[{"toolName":"Bash"}]}]"#)]);
    s.answer(Choice::Always);
    assert_eq!(s.mode, "acceptEdits", "remembering a rule is not a mode change");
}

#[test]
fn a_subagent_report_is_the_agent_calls_result_whether_it_ran_in_the_background_or_not() {
    let report = |s: &Session, i: usize| match &s.items[i].entry {
        Entry::Tool { result: Some((r, e)), .. } => (r.clone(), *e),
        _ => panic!("no result"),
    };
    let mut s = Session::new(1, String::new());
    feed(
        &mut s,
        &[
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m","content":[{"type":"tool_use","id":"a1","name":"Agent","input":{"description":"count"}}]}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"a1","content":[{"type":"text","text":"Async agent launched successfully. agentId: x"}]}]}}"#,
            r#"{"type":"result","subtype":"success"}"#,
        ],
    );
    assert_eq!(report(&s, 0).0, BACKGROUND);
    assert!(!s.busy, "a subagent working after the turn does not make the tab busy");
    feed(
        &mut s,
        &[
            r#"{"type":"assistant","parent_tool_use_id":"a1","message":{"id":"n","content":[{"type":"tool_use","id":"h","name":"SubagentHandback","input":{"message":"2 lines\n"}}]}}"#,
            r#"{"type":"system","subtype":"task_notification","tool_use_id":"a1","status":"completed","summary":"delivered","handback_report":{"text":"2 lines"}}"#,
        ],
    );
    assert_eq!(s.items.len(), 1, "the handback is not listed as a step");
    assert_eq!(report(&s, 0), ("2 lines".into(), false));
    // In the foreground the report comes first and the tool result is only a receipt.
    feed(
        &mut s,
        &[
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m2","content":[{"type":"tool_use","id":"a2","name":"Agent","input":{}}]}}"#,
            r#"{"type":"assistant","parent_tool_use_id":"a2","message":{"id":"n2","content":[{"type":"tool_use","id":"h2","name":"SubagentHandback","input":{"message":"done"}}]}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"a2","content":"This agent's report was delivered to you as a message"}]}}"#,
            r#"{"type":"system","subtype":"task_notification","tool_use_id":"a3","status":"failed","summary":"boom"}"#,
        ],
    );
    assert_eq!(report(&s, 1), ("done".into(), false));
    // A background agent that ends without a report: its summary, not "running" forever.
    feed(
        &mut s,
        &[
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m3","content":[{"type":"tool_use","id":"a4","name":"Agent","input":{}}]}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"a4","content":"Async agent launched successfully."}]}}"#,
            r#"{"type":"system","subtype":"task_notification","tool_use_id":"a4","status":"completed","summary":"Agent finished"}"#,
        ],
    );
    assert_eq!(report(&s, 2), ("Agent finished".into(), false));
}

#[test]
fn a_huge_tool_result_is_kept_only_in_part_and_never_cut_inside_a_character() {
    let mut s = Session::new(1, String::new());
    s.handle(&serde_json::from_str(r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"x"}}]}}"#).unwrap());
    let big = "é".repeat(MAX_RESULT); // 2 bytes each: the cut falls inside a character
    s.set_result("t1", big, false);
    let Entry::Tool { result: Some((r, _)), .. } = &s.items[0].entry else { panic!("no result") };
    assert!(r.len() < MAX_RESULT + 100 && r.ends_with("KB more not kept here"), "{}", &r[r.len() - 40..]);
    s.set_result("t1", "small".into(), false);
    assert!(matches!(&s.items[0].entry, Entry::Tool { result: Some((r, _)), .. } if r == "small"));
}
