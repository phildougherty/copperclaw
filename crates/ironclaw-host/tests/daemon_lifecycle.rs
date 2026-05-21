//! Integration test for the `ironclaw start` / `stop` / `status` /
//! `logs` lifecycle. Drives the freshly-built `ironclaw` binary as a
//! subprocess so we exercise the real re-exec dance, not just the
//! library helpers.
//!
//! Skipped when the binary isn't built yet (e.g. `cargo test -p
//! ironclaw-host --lib` from a cold checkout). `cargo test --workspace`
//! and CI always build it as a normal dev-dependency of this test
//! crate by virtue of `[[bin]]` in the host crate's Cargo.toml.

use std::process::Command;
use std::time::{Duration, Instant};

/// Path to the freshly-built `ironclaw` binary that Cargo provides
/// to integration tests for the bin we're testing.
fn ironclaw_bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironclaw")
}

/// Spawn `ironclaw <args>` with `IRONCLAW_DATA_DIR=tmp` and capture
/// the result. Each call inherits a clean env so the platform-default
/// install dir doesn't shadow our test-only data root.
fn run(tmp: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(ironclaw_bin())
        .args(args)
        // Empty channels list so the host doesn't try to bind a real
        // cli channel's FIFO during this test — we just want a socket
        // and a clean shutdown.
        .env("IRONCLAW_DATA_DIR", tmp)
        .env("IRONCLAW_CHANNELS", "")
        .env("IRONCLAW_LOG", "warn")
        .env_remove("IRONCLAW_ICLAW_SOCKET")
        .output()
        .expect("spawn ironclaw")
}

/// Wait until `pred()` is true or `deadline` elapses. Returns true if
/// the predicate fired in time.
fn wait_until<F: Fn() -> bool>(deadline: Duration, pred: F) -> bool {
    let stop = Instant::now() + deadline;
    while Instant::now() < stop {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn status_when_not_running_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["status"]);
    assert!(!out.status.success(), "status should fail when not running");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("not running"),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn status_json_when_not_running_has_running_false() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["status", "--json"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("not json: {stdout}"));
    assert_eq!(v["running"], false);
    assert_eq!(v["pid"], serde_json::Value::Null);
}

#[test]
fn logs_when_no_log_file_errors_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["logs"]);
    assert!(!out.status.success(), "logs should fail when log absent");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("log file") || stderr.contains("ironclaw.log"),
        "stderr: {stderr}",
    );
}

#[test]
fn stop_when_not_running_default_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["stop"]);
    assert!(
        out.status.success(),
        "stop without --strict should exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("not running"), "stdout: {stdout}");
}

#[test]
fn stop_when_not_running_strict_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(tmp.path(), &["stop", "--strict"]);
    assert!(!out.status.success(), "stop --strict should fail");
}

/// Full start → status → stop round trip. Skipped on systems without
/// a container runtime configured because `ironclaw run` aborts on
/// runtime detect failure (boot error code 3). We try the round trip
/// and skip on that specific exit code so non-Docker dev machines
/// don't flake the suite.
#[test]
fn start_then_stop_round_trip() {
    let tmp = tempfile::tempdir().unwrap();

    // Kick off `ironclaw start`. With `IRONCLAW_CHANNELS=""` and no
    // image tag the host still tries to detect a container runtime.
    // If detection fails the daemon exits and `start` will time out
    // waiting for the socket — we treat that as a skip rather than a
    // failure so the test is robust on dev boxes without Docker.
    let out = run(tmp.path(), &["start"]);

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // exit code 1 == StartTimeout (host crashed during boot,
        // typically because container runtime detection failed).
        // Skip the test in that case.
        if out.status.code() == Some(1)
            && (stderr.contains("did not become ready")
                || stderr.contains("runtime"))
        {
            eprintln!("skipping: start timeout (no container runtime?); {stderr}");
            return;
        }
        panic!("ironclaw start failed: {stderr}");
    }

    let pid_path = tmp.path().join("ironclaw.pid");
    assert!(pid_path.exists(), "pid file should be written");

    // status should report running + exit 0.
    let st = run(tmp.path(), &["status"]);
    assert!(
        st.status.success(),
        "status should exit 0; stdout: {} stderr: {}",
        String::from_utf8_lossy(&st.stdout),
        String::from_utf8_lossy(&st.stderr),
    );
    assert!(String::from_utf8_lossy(&st.stdout).contains("running"));

    // logs should now succeed (the daemon writes its banner to the log
    // file before idling).
    let lg = run(tmp.path(), &["logs", "-n", "10"]);
    assert!(
        lg.status.success(),
        "logs should exit 0; stderr: {}",
        String::from_utf8_lossy(&lg.stderr),
    );

    // stop should reap the daemon.
    let sp = run(tmp.path(), &["stop"]);
    assert!(
        sp.status.success(),
        "stop should exit 0; stderr: {}",
        String::from_utf8_lossy(&sp.stderr),
    );

    // pid file should be removed.
    assert!(
        wait_until(Duration::from_secs(3), || !pid_path.exists()),
        "pid file should be removed after stop",
    );
}
