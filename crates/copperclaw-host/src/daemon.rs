//! Daemon-style lifecycle subcommands: `start`, `stop`, `status`, `logs`.
//!
//! These wrap the existing `copperclaw run` command into a one-terminal flow.
//! `start` re-execs the current binary with `--__daemonized run` in a
//! detached process group, redirecting stdio into a log file under the
//! data dir; `stop` reads the PID file and sends `SIGTERM` (escalating
//! to `SIGKILL` after a grace period); `status` reports the runtime
//! state; `logs` tails the log file.
//!
//! Signal delivery is shelled out to `kill(1)` because the workspace
//! forbids `unsafe_code`, which rules out direct `libc::kill` FFI.
//! Process-group detach uses `std::os::unix::process::CommandExt::process_group`
//! (stable, safe). This is the "nohup-style" detach the brief asks for —
//! a full POSIX double-fork would need unsafe, and isn't necessary for a
//! locally-launched daemon (the parent shell can't reach the child via
//! SIGHUP once it's in a fresh process group, and stdio is fully
//! redirected to files).

use crate::config::HostConfig;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Filename for the PID file inside the data dir.
pub const PID_FILE_NAME: &str = "copperclaw.pid";

/// Subdirectory under the data dir for log files.
pub const LOG_DIR_NAME: &str = "logs";

/// Filename for the host log inside `<data_dir>/logs/`.
pub const LOG_FILE_NAME: &str = "copperclaw.log";

/// Internal marker env var: set on the child of `copperclaw start` so the
/// re-exec'd process knows it should run `copperclaw run` instead of
/// re-entering the spawn dance.
pub const DAEMONIZED_ENV: &str = "COPPERCLAW_DAEMONIZED";

/// Maximum time `start` waits for the host's admin socket to appear
/// before giving up.
pub const START_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time `stop` waits for graceful shutdown before escalating
/// to SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// Resolved on-disk paths for daemon lifecycle.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub data_dir: PathBuf,
    pub pid_file: PathBuf,
    pub log_dir: PathBuf,
    pub log_file: PathBuf,
    pub socket: PathBuf,
}

impl DaemonPaths {
    /// Derive lifecycle paths from a [`HostConfig`].
    pub fn from_config(cfg: &HostConfig) -> Self {
        Self::for_data_dir(cfg.data_dir(), &cfg.ncl_socket_path)
    }

    /// Pure-function variant; lets tests synthesise paths without a full
    /// `HostConfig`.
    pub fn for_data_dir(data_dir: &Path, socket: &Path) -> Self {
        let pid_file = data_dir.join(PID_FILE_NAME);
        let log_dir = data_dir.join(LOG_DIR_NAME);
        let log_file = log_dir.join(LOG_FILE_NAME);
        Self {
            data_dir: data_dir.to_path_buf(),
            pid_file,
            log_dir,
            log_file,
            socket: socket.to_path_buf(),
        }
    }
}

/// Errors surfaced from the daemon-lifecycle commands.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// The PID file already exists and the recorded PID is alive.
    #[error("copperclaw already running (pid {0}); run `copperclaw status` or `copperclaw stop`")]
    AlreadyRunning(i32),
    /// The host did not bind its socket within [`START_READY_TIMEOUT`].
    #[error(
        "copperclaw start: host did not become ready within {timeout:?}; \
         check {log}"
    )]
    StartTimeout {
        timeout: Duration,
        log: PathBuf,
    },
    /// I/O error while reading/writing PID file or log.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    /// PID file existed but the recorded PID was not alive — strict mode.
    #[error("copperclaw not running (stale pid {0})")]
    StalePid(i32),
    /// `--strict` requested but no PID file present.
    #[error("copperclaw not running")]
    NotRunning,
}

impl DaemonError {
    /// Process exit code to use for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::AlreadyRunning(_) | Self::StartTimeout { .. } => 1,
            Self::Io { .. } => 2,
            Self::StalePid(_) | Self::NotRunning => 3,
        }
    }

    fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Read the PID file. Returns `None` if it doesn't exist (or is empty /
/// malformed — those are treated like "no PID" so a corrupt file
/// doesn't wedge `stop`).
pub fn read_pid_file(path: &Path) -> Result<Option<i32>, DaemonError> {
    let bytes = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(DaemonError::io(format!("read {}", path.display()), e)),
    };
    Ok(bytes.trim().parse::<i32>().ok())
}

/// Write `pid` to the PID file, creating parents if needed.
pub fn write_pid_file(path: &Path, pid: i32) -> Result<(), DaemonError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| DaemonError::io(format!("create {}", parent.display()), e))?;
    }
    std::fs::write(path, format!("{pid}\n"))
        .map_err(|e| DaemonError::io(format!("write {}", path.display()), e))
}

/// Remove the PID file. `NotFound` is swallowed.
pub fn remove_pid_file(path: &Path) -> Result<(), DaemonError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(DaemonError::io(format!("remove {}", path.display()), e)),
    }
}

/// `kill -0 pid` semantics — true when the process is alive.
///
/// Shells out to `/bin/kill` (or `kill` on PATH) because we can't call
/// `libc::kill` without unsafe.
pub fn pid_is_alive(pid: i32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Send a signal by name (e.g. `"TERM"`, `"KILL"`). Returns true on a
/// zero exit from `kill`.
pub fn send_signal(pid: i32, sig: &str) -> bool {
    Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Daemonize: re-exec the current binary so the host runs detached in its
/// own process group. Returns the child PID on success.
///
/// The child inherits `COPPERCLAW_DAEMONIZED=1` and `COPPERCLAW_DATA_DIR`
/// (when set) so that env-based config resolution sees the same values
/// the foreground parent did. `main` checks the marker and calls
/// `boot::run_host` directly instead of recursing through this fn.
fn spawn_detached(
    paths: &DaemonPaths,
    extra_args: &[String],
) -> Result<u32, DaemonError> {
    use std::os::unix::process::CommandExt as _;

    // Ensure data dir + log dir exist before redirecting child stdio
    // into the log file (`File::create` would fail otherwise).
    std::fs::create_dir_all(&paths.log_dir)
        .map_err(|e| DaemonError::io(format!("create {}", paths.log_dir.display()), e))?;

    // Open the log file for append. The child inherits the FDs.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)
        .map_err(|e| DaemonError::io(format!("open {}", paths.log_file.display()), e))?;
    let log_clone = log
        .try_clone()
        .map_err(|e| DaemonError::io("dup log fd", e))?;

    let exe = std::env::current_exe()
        .map_err(|e| DaemonError::io("current_exe", e))?;

    let mut cmd = Command::new(exe);
    cmd.arg("run");
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.env(DAEMONIZED_ENV, "1");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(log_clone));
    // `process_group(0)` puts the child in a new process group so the
    // controlling terminal's SIGHUP/SIGINT can't reach it. Stable since
    // Rust 1.64, no unsafe required.
    cmd.process_group(0);

    let child = cmd
        .spawn()
        .map_err(|e| DaemonError::io("spawn daemon", e))?;
    Ok(child.id())
}

/// Wait for the host's admin socket to appear, polling at ~50ms.
///
/// Returns Ok on success, Err on timeout. We poll the filesystem rather
/// than dialling the socket because the connect attempt would race with
/// the server's `bind` / `listen` calls; the file's mere existence is a
/// monotonic signal that `bind` has returned.
pub fn wait_for_socket(socket: &Path, timeout: Duration) -> Result<(), DaemonError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(DaemonError::StartTimeout {
        timeout,
        log: socket.with_file_name(LOG_FILE_NAME),
    })
}

/// `copperclaw start` — spawn detached, wait for ready, write PID file.
///
/// `extra_args` are passed through to the child's `copperclaw run`
/// invocation (e.g. `--env-file`).
pub fn cmd_start(
    cfg: &HostConfig,
    extra_args: &[String],
) -> Result<StartOutcome, DaemonError> {
    let paths = DaemonPaths::from_config(cfg);

    // Refuse if an alive PID is recorded.
    if let Some(pid) = read_pid_file(&paths.pid_file)? {
        if pid_is_alive(pid) {
            return Err(DaemonError::AlreadyRunning(pid));
        }
        // Stale — clean up and proceed.
        remove_pid_file(&paths.pid_file)?;
    }

    // Make sure the data dir exists before we hand off so `COPPERCLAW_DATA_DIR`
    // is meaningful in the child's HostConfig::from_env path.
    std::fs::create_dir_all(&paths.data_dir)
        .map_err(|e| DaemonError::io(format!("create {}", paths.data_dir.display()), e))?;

    let pid = spawn_detached(&paths, extra_args)?;
    // Convert u32 -> i32 with saturating cast; PIDs fit in i32 on every
    // unix we support.
    let pid_i32 = i32::try_from(pid).unwrap_or(i32::MAX);
    write_pid_file(&paths.pid_file, pid_i32)?;

    // Block on socket-ready so the operator sees a useful prompt return.
    match wait_for_socket(&paths.socket, START_READY_TIMEOUT) {
        Ok(()) => Ok(StartOutcome {
            pid: pid_i32,
            socket: paths.socket,
            log: paths.log_file,
        }),
        Err(e) => {
            // The child may have crashed. Leave the PID file in place
            // so the operator can investigate; surface the error.
            Err(e)
        }
    }
}

/// Result of a successful [`cmd_start`].
#[derive(Debug, Clone)]
pub struct StartOutcome {
    pub pid: i32,
    pub socket: PathBuf,
    pub log: PathBuf,
}

/// `copperclaw stop` — read PID file, send TERM, wait, escalate to KILL.
///
/// Returns the path taken so callers can print which signal landed it.
pub fn cmd_stop(cfg: &HostConfig, strict: bool) -> Result<StopOutcome, DaemonError> {
    let paths = DaemonPaths::from_config(cfg);
    let Some(pid) = read_pid_file(&paths.pid_file)? else {
        return if strict {
            Err(DaemonError::NotRunning)
        } else {
            Ok(StopOutcome::NotRunning)
        };
    };
    if !pid_is_alive(pid) {
        // PID file present but process gone — clean up.
        remove_pid_file(&paths.pid_file)?;
        return if strict {
            Err(DaemonError::StalePid(pid))
        } else {
            Ok(StopOutcome::StalePidCleared(pid))
        };
    }

    // Send SIGTERM and poll.
    if !send_signal(pid, "TERM") {
        return Err(DaemonError::io(
            format!("kill -TERM {pid}"),
            io::Error::other("kill exited non-zero"),
        ));
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            remove_pid_file(&paths.pid_file)?;
            return Ok(StopOutcome::Graceful(pid));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Escalate to SIGKILL.
    let _ = send_signal(pid, "KILL");
    // Give the kernel a moment to reap.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    remove_pid_file(&paths.pid_file)?;
    Ok(StopOutcome::Killed(pid))
}

/// Outcome of [`cmd_stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// No PID file — host wasn't running. Returned in non-strict mode.
    NotRunning,
    /// PID file existed but recorded PID was gone; we cleared the file.
    StalePidCleared(i32),
    /// SIGTERM was enough.
    Graceful(i32),
    /// SIGKILL escalation fired.
    Killed(i32),
}

/// `copperclaw status` — describe what's running.
///
/// Returns a structured snapshot; callers render it as text or JSON.
pub fn cmd_status(cfg: &HostConfig) -> Result<StatusSnapshot, DaemonError> {
    let paths = DaemonPaths::from_config(cfg);
    let pid_opt = read_pid_file(&paths.pid_file)?;
    let (running, pid, started_at) = if let Some(p) = pid_opt {
        if pid_is_alive(p) {
            // Approximate started_at as the PID file's mtime — it was
            // written at start. Best-effort: if the metadata read
            // fails we leave it as None.
            let started = std::fs::metadata(&paths.pid_file)
                .ok()
                .and_then(|m| m.modified().ok());
            (true, Some(p), started)
        } else {
            (false, Some(p), None)
        }
    } else {
        (false, None, None)
    };
    let uptime = started_at.and_then(|s| s.elapsed().ok());
    let active_sessions = if running {
        count_active_sessions(cfg)
    } else {
        None
    };
    Ok(StatusSnapshot {
        running,
        pid,
        uptime,
        data_dir: paths.data_dir,
        socket: paths.socket,
        log_file: paths.log_file,
        active_sessions,
    })
}

/// Try to count active sessions by opening the central DB. Returns
/// `None` if the DB isn't reachable (e.g. host crashed mid-boot).
fn count_active_sessions(cfg: &HostConfig) -> Option<usize> {
    let path = cfg.central_db_path();
    if !path.exists() {
        return None;
    }
    let db = copperclaw_db::central::CentralDb::open(&path).ok()?;
    let rows = copperclaw_db::tables::sessions::list_active(&db).ok()?;
    Some(rows.len())
}

/// Snapshot of `copperclaw status`.
#[derive(Debug, Clone)]
pub struct StatusSnapshot {
    pub running: bool,
    pub pid: Option<i32>,
    pub uptime: Option<Duration>,
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    pub log_file: PathBuf,
    pub active_sessions: Option<usize>,
}

impl StatusSnapshot {
    /// Render as a one-screen text block.
    pub fn render_text(&self) -> String {
        let mut s = String::new();
        if self.running {
            s.push_str("copperclaw: running\n");
        } else {
            s.push_str("copperclaw: not running\n");
        }
        if let Some(pid) = self.pid {
            s.push_str(&format!("  pid:      {pid}\n"));
        }
        if let Some(up) = self.uptime {
            s.push_str(&format!("  uptime:   {}\n", format_duration(up)));
        }
        s.push_str(&format!("  data:     {}\n", self.data_dir.display()));
        s.push_str(&format!("  socket:   {}\n", self.socket.display()));
        s.push_str(&format!("  log:      {}\n", self.log_file.display()));
        if let Some(n) = self.active_sessions {
            s.push_str(&format!("  sessions: {n} active\n"));
        }
        s
    }

    /// Render as machine-friendly JSON.
    pub fn render_json(&self) -> serde_json::Value {
        serde_json::json!({
            "running": self.running,
            "pid": self.pid,
            "uptime_secs": self.uptime.map(|d| d.as_secs()),
            "data_dir": self.data_dir.display().to_string(),
            "socket": self.socket.display().to_string(),
            "log_file": self.log_file.display().to_string(),
            "active_sessions": self.active_sessions,
        })
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// `copperclaw logs` — print the last `tail` lines, optionally follow.
///
/// When `follow` is true the call blocks until the writer is interrupted
/// (Ctrl-C) or the file is removed/rotated. The default tail count is
/// 50 lines.
pub fn cmd_logs(cfg: &HostConfig, tail: usize, follow: bool) -> Result<(), DaemonError> {
    let paths = DaemonPaths::from_config(cfg);
    if !paths.log_file.exists() {
        return Err(DaemonError::io(
            format!("read {}", paths.log_file.display()),
            io::Error::new(
                io::ErrorKind::NotFound,
                "log file does not exist; has the host been started?",
            ),
        ));
    }
    let mut file = std::fs::File::open(&paths.log_file)
        .map_err(|e| DaemonError::io(format!("open {}", paths.log_file.display()), e))?;

    // Read the entire file, keep the last `tail` lines.
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .map_err(|e| DaemonError::io(format!("read {}", paths.log_file.display()), e))?;
    let lines: Vec<&str> = buf.lines().collect();
    let start = lines.len().saturating_sub(tail);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in &lines[start..] {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();

    if !follow {
        return Ok(());
    }

    // Tail loop. We don't use inotify because polling is simpler and
    // matches `tail -f` for the small log sizes involved.
    drop(out);
    let mut pos = file
        .seek(SeekFrom::End(0))
        .map_err(|e| DaemonError::io("seek log", e))?;
    let stdout = io::stdout();
    loop {
        let meta = std::fs::metadata(&paths.log_file)
            .map_err(|e| DaemonError::io("stat log", e))?;
        if meta.len() < pos {
            // File got rotated/truncated — rewind.
            pos = 0;
            file = std::fs::File::open(&paths.log_file)
                .map_err(|e| DaemonError::io("reopen log", e))?;
        }
        if meta.len() > pos {
            let _ = file.seek(SeekFrom::Start(pos));
            let mut reader = io::BufReader::new(&file);
            let mut chunk = String::new();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => chunk.push_str(&line),
                }
            }
            if !chunk.is_empty() {
                let mut out = stdout.lock();
                let _ = out.write_all(chunk.as_bytes());
                let _ = out.flush();
            }
            pos = meta.len();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_from_data_dir_layout() {
        let p = DaemonPaths::for_data_dir(
            Path::new("/tmp/copperclaw"),
            Path::new("/tmp/copperclaw/cclaw.sock"),
        );
        assert_eq!(p.pid_file, PathBuf::from("/tmp/copperclaw/copperclaw.pid"));
        assert_eq!(p.log_dir, PathBuf::from("/tmp/copperclaw/logs"));
        assert_eq!(
            p.log_file,
            PathBuf::from("/tmp/copperclaw/logs/copperclaw.log")
        );
    }

    #[test]
    fn pid_file_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("copperclaw.pid");
        write_pid_file(&pid_path, 12345).unwrap();
        assert_eq!(read_pid_file(&pid_path).unwrap(), Some(12345));
        remove_pid_file(&pid_path).unwrap();
        assert_eq!(read_pid_file(&pid_path).unwrap(), None);
    }

    #[test]
    fn read_pid_file_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("nope.pid");
        assert_eq!(read_pid_file(&p).unwrap(), None);
    }

    #[test]
    fn read_pid_file_malformed_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("bad.pid");
        std::fs::write(&p, "notanumber").unwrap();
        // Malformed PID is treated as "no PID" so a stale, corrupt
        // file doesn't wedge stop/status.
        assert_eq!(read_pid_file(&p).unwrap(), None);
    }

    #[test]
    fn remove_pid_file_missing_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        remove_pid_file(&tmp.path().join("never.pid")).unwrap();
    }

    #[test]
    fn pid_alive_for_self_is_true() {
        // The test process is by definition alive AND signalable by
        // itself, so `kill -0 <self>` succeeds in any sandbox.
        // (Using PID 1 would flake in unprivileged containers where
        // EPERM masks the truth.)
        let pid = i32::try_from(std::process::id()).unwrap();
        assert!(pid_is_alive(pid));
    }

    #[test]
    fn pid_alive_for_sentinel_is_false() {
        // Use a PID known to never exist. i32::MAX is reserved on
        // every unix kernel we care about.
        assert!(!pid_is_alive(i32::MAX - 1));
    }

    #[test]
    fn status_snapshot_text_includes_paths() {
        let snap = StatusSnapshot {
            running: false,
            pid: None,
            uptime: None,
            data_dir: PathBuf::from("/srv/copperclaw"),
            socket: PathBuf::from("/srv/copperclaw/cclaw.sock"),
            log_file: PathBuf::from("/srv/copperclaw/logs/copperclaw.log"),
            active_sessions: None,
        };
        let text = snap.render_text();
        assert!(text.contains("not running"));
        assert!(text.contains("/srv/copperclaw"));
        assert!(text.contains("cclaw.sock"));
    }

    #[test]
    fn status_snapshot_text_running_includes_pid_uptime() {
        let snap = StatusSnapshot {
            running: true,
            pid: Some(4242),
            uptime: Some(Duration::from_secs(3725)),
            data_dir: PathBuf::from("/d"),
            socket: PathBuf::from("/d/s"),
            log_file: PathBuf::from("/d/l"),
            active_sessions: Some(3),
        };
        let text = snap.render_text();
        assert!(text.contains("running"));
        assert!(text.contains("4242"));
        assert!(text.contains("1h2m5s"));
        assert!(text.contains("3 active"));
    }

    #[test]
    fn status_snapshot_json_shape() {
        let snap = StatusSnapshot {
            running: true,
            pid: Some(7),
            uptime: Some(Duration::from_secs(30)),
            data_dir: PathBuf::from("/d"),
            socket: PathBuf::from("/d/s"),
            log_file: PathBuf::from("/d/l"),
            active_sessions: Some(0),
        };
        let v = snap.render_json();
        assert_eq!(v["running"], true);
        assert_eq!(v["pid"], 7);
        assert_eq!(v["uptime_secs"], 30);
        assert_eq!(v["active_sessions"], 0);
    }

    #[test]
    fn cmd_status_reports_not_running_when_no_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        let snap = cmd_status(&cfg).unwrap();
        assert!(!snap.running);
        assert!(snap.pid.is_none());
    }

    #[test]
    fn cmd_status_reports_stale_pid_as_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        // Use a PID guaranteed not to exist.
        write_pid_file(&cfg.data_dir().join(PID_FILE_NAME), i32::MAX - 2).unwrap();
        let snap = cmd_status(&cfg).unwrap();
        assert!(!snap.running);
        assert_eq!(snap.pid, Some(i32::MAX - 2));
    }

    #[test]
    fn cmd_stop_without_pid_file_returns_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        let out = cmd_stop(&cfg, false).unwrap();
        assert_eq!(out, StopOutcome::NotRunning);
    }

    #[test]
    fn cmd_stop_strict_without_pid_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        let err = cmd_stop(&cfg, true).unwrap_err();
        assert!(matches!(err, DaemonError::NotRunning));
    }

    #[test]
    fn cmd_stop_clears_stale_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        let pid_path = cfg.data_dir().join(PID_FILE_NAME);
        write_pid_file(&pid_path, i32::MAX - 3).unwrap();
        let out = cmd_stop(&cfg, false).unwrap();
        assert!(matches!(out, StopOutcome::StalePidCleared(_)));
        assert!(!pid_path.exists());
    }

    #[test]
    fn cmd_logs_errors_when_log_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        let err = cmd_logs(&cfg, 50, false).unwrap_err();
        match err {
            DaemonError::Io { source, .. } => {
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn cmd_logs_prints_last_n_lines_no_follow() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        // Pre-create the log file with 5 lines.
        let log_dir = cfg.data_dir().join(LOG_DIR_NAME);
        std::fs::create_dir_all(&log_dir).unwrap();
        let log = log_dir.join(LOG_FILE_NAME);
        std::fs::write(&log, "a\nb\nc\nd\ne\n").unwrap();
        // We don't capture stdout here — just confirm the call succeeds.
        cmd_logs(&cfg, 3, false).unwrap();
    }

    #[test]
    fn format_duration_renders_h_m_s() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m5s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h2m5s");
    }

    #[test]
    fn daemon_error_exit_codes() {
        assert_eq!(DaemonError::AlreadyRunning(1).exit_code(), 1);
        assert_eq!(
            DaemonError::StartTimeout {
                timeout: Duration::from_secs(1),
                log: PathBuf::from("/x"),
            }
            .exit_code(),
            1
        );
        assert_eq!(
            DaemonError::Io {
                context: "x".into(),
                source: io::Error::other("y"),
            }
            .exit_code(),
            2
        );
        assert_eq!(DaemonError::StalePid(7).exit_code(), 3);
        assert_eq!(DaemonError::NotRunning.exit_code(), 3);
    }

    #[test]
    fn cmd_start_refuses_when_alive_pid_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            ..HostConfig::default()
        };
        std::fs::create_dir_all(cfg.data_dir()).unwrap();
        // Use the test process's own PID — guaranteed alive and
        // signalable by itself even in unprivileged sandboxes where
        // `kill -0 1` would fail with EPERM.
        let self_pid = i32::try_from(std::process::id()).unwrap();
        write_pid_file(&cfg.data_dir().join(PID_FILE_NAME), self_pid).unwrap();
        let err = cmd_start(&cfg, &[]).unwrap_err();
        match err {
            DaemonError::AlreadyRunning(p) => assert_eq!(p, self_pid),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }
}
