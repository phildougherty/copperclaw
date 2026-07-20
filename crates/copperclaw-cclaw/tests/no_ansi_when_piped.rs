//! D1 acceptance: `cclaw doctor | cat` (i.e. any piped invocation) must
//! produce zero ANSI escape codes, and `NO_COLOR=1` must suppress color
//! even where a terminal is present.
//!
//! These tests spawn the real `cclaw` binary with piped stdio, which is
//! exactly the "piped" case: `std::io::stdout().is_terminal()` is false
//! in the child, so the style layer must stay silent. The socket points
//! at a nonexistent path so `doctor` renders its FAIL row (with a `fix:`
//! hint) without needing a live host.

use std::process::Command;

fn cclaw() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cclaw"))
}

fn assert_no_ansi(label: &str, bytes: &[u8]) {
    assert!(
        !bytes.contains(&0x1b),
        "{label} contains ANSI escape bytes: {:?}",
        String::from_utf8_lossy(bytes)
    );
}

#[test]
fn piped_doctor_output_is_ansi_free() {
    let out = cclaw()
        .args(["--socket", "/nonexistent/cclaw-test.sock", "doctor"])
        .env_remove("NO_COLOR")
        .env_remove("CCLAW_AGENT_CALLER")
        .output()
        .expect("spawn cclaw");
    // Doctor with an unreachable host exits non-zero but still renders
    // the FAIL row + fix hint — the styled path.
    assert_no_ansi("stdout", &out.stdout);
    assert_no_ansi("stderr", &out.stderr);
    let text =
        String::from_utf8_lossy(&out.stderr).to_string() + &String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("host-reachable"),
        "expected the doctor host-reachable check in output: {text:?}"
    );
    assert!(
        text.contains("fix:"),
        "expected a fix: hint in output: {text:?}"
    );
}

#[test]
fn no_color_env_suppresses_ansi() {
    let out = cclaw()
        .args(["--socket", "/nonexistent/cclaw-test.sock", "doctor"])
        .env("NO_COLOR", "1")
        .env_remove("CCLAW_AGENT_CALLER")
        .output()
        .expect("spawn cclaw");
    assert_no_ansi("stdout", &out.stdout);
    assert_no_ansi("stderr", &out.stderr);
}

#[test]
fn no_color_flag_suppresses_ansi() {
    let out = cclaw()
        .args([
            "--no-color",
            "--socket",
            "/nonexistent/cclaw-test.sock",
            "doctor",
        ])
        .env_remove("NO_COLOR")
        .env_remove("CCLAW_AGENT_CALLER")
        .output()
        .expect("spawn cclaw");
    assert_no_ansi("stdout", &out.stdout);
    assert_no_ansi("stderr", &out.stderr);
}

#[test]
fn piped_chat_output_is_ansi_free() {
    use copperclaw_channels_cli::CliFrame;
    use copperclaw_channels_core::{
        Breadcrumb, Card, CardField, DiffCard, DiffHunk, DiffLine, DiffLineKind, ErrorCard,
        ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList, TodoListItem,
    };

    // One frame of every kind, serialized with the real wire type, plus
    // a legacy pre-JSONL line — the sniff contract's other half.
    let frames = vec![
        CliFrame::Chat {
            text: "hello from the agent".into(),
            files: vec!["report.txt".into()],
        },
        CliFrame::Card {
            card: Card {
                title: Some("Deploy report".into()),
                body: Some("all green".into()),
                fields: vec![CardField {
                    label: "env".into(),
                    value: "prod".into(),
                    inline: false,
                }],
                buttons: vec![],
                image_url: None,
            },
        },
        CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("shell")
                .with_detail("cargo check")
                .finished(true, Some("passed in 3.1s".into())),
        },
        CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("task").with_steps(vec![
                Breadcrumb::running("edit_file")
                    .with_detail("src/lib.rs")
                    .finished(true, Some("wrote 12 lines".into())),
            ]),
        },
        CliFrame::Diff {
            diff: DiffCard {
                path: "src/main.rs".into(),
                language: Some("rust".into()),
                hunks: vec![DiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Remove,
                            text: "old_line();".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Add,
                            text: "new_line();".into(),
                        },
                    ],
                }],
                added: 1,
                removed: 1,
                truncated: false,
            },
        },
        CliFrame::Collapsible {
            text: "full body here".into(),
            summary: "1 line of output".into(),
            preview_lines: vec![],
        },
        CliFrame::TodoList {
            todo_list: TodoList {
                title: Some("Plan".into()),
                items: vec![
                    TodoListItem {
                        id: 1,
                        text: "write tests".into(),
                        status: TodoItemStatus::Completed,
                        blocked_reason: None,
                    },
                    TodoListItem {
                        id: 2,
                        text: "run gate".into(),
                        status: TodoItemStatus::InProgress,
                        blocked_reason: None,
                    },
                ],
            },
        },
        CliFrame::Error {
            error: ErrorCard {
                title: "Something went wrong".into(),
                summary: "provider rate limited".into(),
                kind: ErrorCardKind::Provider,
                details: None,
                retryable: true,
            },
        },
        CliFrame::Thinking {
            thinking: ThinkingBlock::visible("pondering the gate"),
        },
    ];
    let mut log = String::new();
    for frame in &frames {
        log.push_str(&serde_json::to_string(frame).expect("serialize frame"));
        log.push('\n');
    }
    log.push_str("agent> legacy plain line\n");

    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("chat.log");
    std::fs::write(&log_path, log).expect("write chat.log");

    // CCLAW_CHAT_REPLAY renders the whole log through the real per-frame
    // path and exits at EOF — no FIFO, no stdin — so the spawned binary
    // exercises exactly the piped-renderer case: is_terminal() is false
    // in the child, so ESC and the \r-repaint must both stay suppressed.
    let out = cclaw()
        .args([
            "chat",
            "--log",
            log_path.to_str().unwrap(),
            "--no-autostart",
        ])
        .env("CCLAW_CHAT_REPLAY", "1")
        .env_remove("NO_COLOR")
        .env_remove("CCLAW_AGENT_CALLER")
        .output()
        .expect("spawn cclaw chat");

    assert!(
        out.status.success(),
        "chat replay must exit 0; stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_no_ansi("stdout", &out.stdout);
    assert_no_ansi("stderr", &out.stderr);
    // The status-line repaint is \r-based; piped stdout must carry no
    // bare carriage return either.
    assert!(
        !out.stdout.contains(&b'\r'),
        "piped chat stdout contains a carriage return: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The rendered text content must survive the styling-off path.
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "hello from the agent",     // chat
        "report.txt",               // chat attachment
        "Deploy report",            // card title
        "env: prod",                // card field
        "shell",                    // breadcrumb tool name
        "cargo check",              // breadcrumb detail
        "passed in 3.1s",           // breadcrumb result
        "edit_file",                // HUD step tool name
        "wrote 12 lines",           // HUD step result
        "-old_line();",             // diff removed line
        "+new_line();",             // diff added line
        "1 line of output",         // collapsible summary
        "full body here",           // collapsible body
        "[x] write tests",          // completed todo
        "[~] run gate",             // in-progress todo
        "provider rate limited",    // error summary
        "pondering the gate",       // thinking
        "agent> legacy plain line", // legacy sniff path
    ] {
        assert!(
            text.contains(expected),
            "expected {expected:?} in piped chat stdout: {text:?}"
        );
    }
}

#[test]
fn json_output_is_ansi_free_and_parseable() {
    let out = cclaw()
        .args([
            "--json",
            "--socket",
            "/nonexistent/cclaw-test.sock",
            "doctor",
        ])
        .env_remove("NO_COLOR")
        .env_remove("CCLAW_AGENT_CALLER")
        .output()
        .expect("spawn cclaw");
    assert_no_ansi("stdout", &out.stdout);
    assert_no_ansi("stderr", &out.stderr);
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).expect("doctor --json must stay parseable JSON");
    assert!(v.get("checks").is_some());
}
