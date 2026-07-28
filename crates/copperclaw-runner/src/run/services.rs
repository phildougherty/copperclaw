//! W2.2 cold-boot service restart hook.
//!
//! Daemons the agent starts (database servers, dev servers) die when the
//! session container idle-stops, while `/data` survives. On runner
//! process startup — once per container boot, before the poll loop —
//! this hook reads `<data_root>/.copperclaw/services` (one bash command
//! per line; blank lines and `#` comments skipped) and runs each line,
//! appending the command's output and exit status with a timestamp to
//! `<data_root>/.copperclaw/services.log`. The agent writes its exact
//! daemon start commands into that file (see the `databases` skill), so
//! persisted daemons restart transparently after a respawn.
//!
//! Guarantees:
//!
//! - **Never fatal**: an unreadable file, spawn error, non-zero exit or
//!   timeout is logged and skipped; the hook returns normally either way.
//! - **Never blocks boot indefinitely**: each command gets a hard
//!   [`PER_COMMAND_TIMEOUT`] and is killed on expiry — the commands are
//!   expected to daemonize and return promptly.
//! - **Once per container boot**: [`run_cold_boot_services`] latches a
//!   process-level flag. The runner process is one-to-one with a
//!   container boot, and the call site sits before the poll loop, so
//!   subsequent turns of the same boot never re-run the file.
//! - **Bounded log**: when `services.log` exceeds [`MAX_LOG_BYTES`] at
//!   hook start it is truncated (with a note) before new entries are
//!   appended, and each command's captured output is capped at
//!   [`MAX_LOGGED_OUTPUT_BYTES`], so the log cannot grow without bound.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;

/// Path of the services file, relative to the data root.
pub(super) const SERVICES_FILE: &str = ".copperclaw/services";
/// Path of the hook's log, relative to the data root.
pub(super) const SERVICES_LOG: &str = ".copperclaw/services.log";
/// Hard wall-clock cap per command. Start commands are expected to
/// daemonize and return; anything still running at the cap is killed
/// and logged as a failure so boot can proceed.
pub(super) const PER_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Size threshold above which `services.log` is truncated at hook
/// start, so repeated respawns cannot grow it without bound.
pub(super) const MAX_LOG_BYTES: u64 = 256 * 1024;
/// Cap on the captured output logged per command.
const MAX_LOGGED_OUTPUT_BYTES: usize = 8 * 1024;

/// What one hook pass did — cheap to assert on in tests, folded into a
/// single `tracing` line in production.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct ServicesReport {
    /// Commands executed (comment / blank lines excluded).
    pub commands: usize,
    /// Commands that failed (spawn error, non-zero exit, or timeout).
    pub failures: usize,
}

/// Production entry point, called from `run_loop` before the poll loop.
/// Resolves the data root the same way the auto-attach seam does and
/// latches a process-level run-once flag.
pub(super) async fn run_cold_boot_services() {
    static RAN: AtomicBool = AtomicBool::new(false);
    run_guarded(
        &RAN,
        &super::project::resolve_data_root(),
        PER_COMMAND_TIMEOUT,
    )
    .await;
}

/// [`run_cold_boot_services`] against an explicit latch, root, and
/// timeout — the testable run-once wrapper. Returns `None` when the
/// latch was already set (nothing ran).
pub(super) async fn run_guarded(
    guard: &AtomicBool,
    data_root: &Path,
    per_command_timeout: Duration,
) -> Option<ServicesReport> {
    if guard.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(run_services_in(data_root, per_command_timeout).await)
}

/// One unguarded hook pass against an explicit data root — the testable
/// core. Infallible by design: every per-command and per-IO failure is
/// logged and skipped.
pub(super) async fn run_services_in(
    data_root: &Path,
    per_command_timeout: Duration,
) -> ServicesReport {
    let mut report = ServicesReport::default();
    let services_path = data_root.join(SERVICES_FILE);
    let contents = match tokio::fs::read_to_string(&services_path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return report,
        Err(err) => {
            tracing::warn!(
                path = %services_path.display(),
                error = %err,
                "services file present but unreadable; skipping cold-boot service restart"
            );
            return report;
        }
    };

    let log_path = data_root.join(SERVICES_LOG);
    rotate_log_if_oversized(&log_path).await;

    for line in contents.lines() {
        let command = line.trim();
        if command.is_empty() || command.starts_with('#') {
            continue;
        }
        report.commands += 1;
        let entry = run_one(command, per_command_timeout).await;
        if !entry.ok {
            report.failures += 1;
            tracing::warn!(command, "cold-boot service command failed; continuing");
        }
        append_log(&log_path, &entry.text).await;
    }

    if report.commands > 0 {
        tracing::info!(
            commands = report.commands,
            failures = report.failures,
            log = %log_path.display(),
            "cold-boot service restart hook completed"
        );
    }
    report
}

/// One executed line's outcome plus its pre-rendered log entry.
struct CommandEntry {
    ok: bool,
    text: String,
}

/// Run a single services line under `bash -c` with a hard timeout,
/// rendering the timestamped log entry. Never errors.
async fn run_one(command: &str, per_command_timeout: Duration) -> CommandEntry {
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut child = tokio::process::Command::new("bash");
    child
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        // If the timeout drops the `output()` future, take the child
        // down with it rather than leaking a stuck process.
        .kill_on_drop(true);

    let (ok, status_line, output) =
        match tokio::time::timeout(per_command_timeout, child.output()).await {
            Ok(Ok(out)) => {
                let mut body = String::from_utf8_lossy(&out.stdout).into_owned();
                body.push_str(&String::from_utf8_lossy(&out.stderr));
                (out.status.success(), out.status.to_string(), body)
            }
            Ok(Err(err)) => (false, format!("spawn failed: {err}"), String::new()),
            Err(_) => (
                false,
                format!("timed out after {per_command_timeout:.0?} (killed)"),
                String::new(),
            ),
        };

    let mut text = format!("[{stamp}] $ {command}\n");
    let capped = cap_output(&output);
    if !capped.is_empty() {
        text.push_str(&capped);
        if !capped.ends_with('\n') {
            text.push('\n');
        }
    }
    text.push_str(&format!("[{stamp}] status: {status_line}\n"));
    CommandEntry { ok, text }
}

/// Truncate captured output to [`MAX_LOGGED_OUTPUT_BYTES`] on a char
/// boundary, with a marker when anything was dropped.
fn cap_output(output: &str) -> String {
    if output.len() <= MAX_LOGGED_OUTPUT_BYTES {
        return output.to_string();
    }
    let mut cut = MAX_LOGGED_OUTPUT_BYTES;
    while !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n...[output truncated]", &output[..cut])
}

/// If the log has grown past [`MAX_LOG_BYTES`], replace it with a
/// single truncation note before this boot's entries are appended.
async fn rotate_log_if_oversized(log_path: &Path) {
    let Ok(meta) = tokio::fs::metadata(log_path).await else {
        return;
    };
    if meta.len() <= MAX_LOG_BYTES {
        return;
    }
    let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let note = format!("[{stamp}] log exceeded {MAX_LOG_BYTES} bytes; truncated\n");
    if let Err(err) = tokio::fs::write(log_path, note).await {
        tracing::warn!(path = %log_path.display(), error = %err, "failed to truncate oversized services log");
    }
}

/// Best-effort append to the services log; a write failure is logged
/// and swallowed (the hook must never be fatal).
async fn append_log(log_path: &Path, text: &str) {
    let open = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .await;
    match open {
        Ok(mut file) => {
            // `flush` matters: a dropped `tokio::fs::File` completes pending
            // writes in a background task, so without it the entry may not
            // be on disk when the next reader (or this boot's next append)
            // looks.
            let write = async {
                file.write_all(text.as_bytes()).await?;
                file.flush().await
            };
            if let Err(err) = write.await {
                tracing::warn!(path = %log_path.display(), error = %err, "failed to append to services log");
            }
        }
        Err(err) => {
            tracing::warn!(path = %log_path.display(), error = %err, "failed to open services log for append");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    fn write_services(root: &Path, body: &str) {
        let dir = root.join(".copperclaw");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("services"), body).unwrap();
    }

    fn read_log(root: &Path) -> String {
        std::fs::read_to_string(root.join(SERVICES_LOG)).unwrap_or_default()
    }

    #[tokio::test]
    async fn absent_services_file_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let report = run_services_in(tmp.path(), Duration::from_secs(5)).await;
        assert_eq!(report, ServicesReport::default());
        assert!(!tmp.path().join(SERVICES_LOG).exists());
    }

    #[tokio::test]
    async fn runs_commands_and_appends_output_and_status() {
        let tmp = tempfile::tempdir().unwrap();
        write_services(tmp.path(), "echo hello-from-services\n");
        let report = run_services_in(tmp.path(), Duration::from_secs(30)).await;
        assert_eq!(
            report,
            ServicesReport {
                commands: 1,
                failures: 0
            }
        );
        let log = read_log(tmp.path());
        assert!(log.contains("$ echo hello-from-services"), "log: {log}");
        assert!(log.contains("hello-from-services\n"), "log: {log}");
        assert!(log.contains("status: exit status: 0"), "log: {log}");
    }

    #[tokio::test]
    async fn comments_and_blank_lines_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_services(
            tmp.path(),
            "# start the database\n\n   \n  echo only-this  \n#echo not-this\n",
        );
        let report = run_services_in(tmp.path(), Duration::from_secs(30)).await;
        assert_eq!(
            report,
            ServicesReport {
                commands: 1,
                failures: 0
            }
        );
        let log = read_log(tmp.path());
        assert_eq!(log.matches("] $ ").count(), 1, "log: {log}");
        assert!(log.contains("$ echo only-this"), "log: {log}");
        assert!(!log.contains("not-this"), "log: {log}");
    }

    #[tokio::test]
    async fn failing_command_is_logged_and_later_commands_still_run() {
        let tmp = tempfile::tempdir().unwrap();
        write_services(tmp.path(), "echo oops >&2; exit 7\necho after-failure\n");
        let report = run_services_in(tmp.path(), Duration::from_secs(30)).await;
        assert_eq!(
            report,
            ServicesReport {
                commands: 2,
                failures: 1
            }
        );
        let log = read_log(tmp.path());
        assert!(log.contains("oops"), "stderr captured: {log}");
        assert!(log.contains("status: exit status: 7"), "log: {log}");
        assert!(log.contains("after-failure"), "log: {log}");
    }

    #[tokio::test]
    async fn hung_command_times_out_and_does_not_block_later_commands() {
        let tmp = tempfile::tempdir().unwrap();
        write_services(tmp.path(), "sleep 30\necho survived\n");
        let report = run_services_in(tmp.path(), Duration::from_millis(200)).await;
        assert_eq!(
            report,
            ServicesReport {
                commands: 2,
                failures: 1
            }
        );
        let log = read_log(tmp.path());
        assert!(log.contains("timed out"), "log: {log}");
        assert!(log.contains("survived"), "log: {log}");
    }

    #[tokio::test]
    async fn guard_latch_runs_only_once() {
        let tmp = tempfile::tempdir().unwrap();
        write_services(tmp.path(), "echo once\n");
        let guard = AtomicBool::new(false);
        let first = run_guarded(&guard, tmp.path(), Duration::from_secs(30)).await;
        assert_eq!(
            first,
            Some(ServicesReport {
                commands: 1,
                failures: 0
            })
        );
        let second = run_guarded(&guard, tmp.path(), Duration::from_secs(30)).await;
        assert_eq!(second, None);
        let log = read_log(tmp.path());
        assert_eq!(log.matches("$ echo once").count(), 1, "log: {log}");
    }

    #[tokio::test]
    async fn oversized_log_is_truncated_before_appending() {
        let tmp = tempfile::tempdir().unwrap();
        write_services(tmp.path(), "echo fresh-entry\n");
        let log_path = tmp.path().join(SERVICES_LOG);
        let oversized = "x".repeat(usize::try_from(MAX_LOG_BYTES).unwrap() + 1);
        std::fs::write(&log_path, &oversized).unwrap();
        run_services_in(tmp.path(), Duration::from_secs(30)).await;
        let log = read_log(tmp.path());
        assert!(!log.contains("xxxx"), "old content dropped");
        assert!(log.contains("truncated"), "log: {log}");
        assert!(log.contains("fresh-entry"), "log: {log}");
        assert!(u64::try_from(log.len()).unwrap() < MAX_LOG_BYTES);
    }

    #[tokio::test]
    async fn per_command_output_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        write_services(tmp.path(), "yes a | head -n 20000\n");
        let report = run_services_in(tmp.path(), Duration::from_secs(30)).await;
        assert_eq!(report.commands, 1);
        let log = read_log(tmp.path());
        assert!(log.contains("...[output truncated]"), "log: {log}");
        assert!(
            log.len() < MAX_LOGGED_OUTPUT_BYTES + 512,
            "log len: {}",
            log.len()
        );
    }
}
