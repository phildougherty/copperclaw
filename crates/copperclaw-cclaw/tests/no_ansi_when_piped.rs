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
