//! Free-disk probing and classification — the shared half.
//!
//! This started life inline in `doctor`'s `disk-space` check. It was
//! factored out when the host needed the *same* thresholds for a very
//! different job: refusing to spawn a session container when the box has
//! no room for one (see
//! `copperclaw_host::container_manager::host_resources`). Two copies of
//! "how full is too full" would drift, and the failure mode of drift here
//! is nasty — `cclaw doctor` reporting OK while the reconcile loop
//! silently refuses every spawn (or vice versa).
//!
//! Everything here is pure except [`statvfs_bytes`], which is the single
//! `statvfs` call both callers share. Callers own their own presentation
//! and their own test seams.

/// One binary gibibyte, the unit the absolute thresholds are expressed in.
pub const GIB: u64 = 1024 * 1024 * 1024;

/// Below this much free space the box is *low* — `doctor` WARNs. Spawns
/// are still allowed at this level: a session container needs far less
/// than 20 GiB, and refusing here would be more disruptive than the risk.
pub const DISK_WARN_BYTES: u64 = 20 * GIB;

/// Below this much free space the box is *critical* — `doctor` FAILs and
/// the host's spawn preflight refuses to launch containers. Docker image
/// layers, the per-session `SQLite` DBs, and the runner's own scratch all
/// want room; under 5 GiB a spawn is a coin flip that usually lands on
/// `ENOSPC` partway through, which is strictly worse than not spawning
/// (the inbound stays pending instead of being burned on a doomed turn).
pub const DISK_FAIL_BYTES: u64 = 5 * GIB;

/// Remediation hint shared by the WARN/FAIL disk rows and by the host's
/// operator alert — one string so the advice cannot drift either.
pub const DISK_FIX: &str = "reclaim space: rotate/delete old host logs under \
    <data>/logs, `docker system prune -af --volumes`, and clear stale \
    Rust `target/` dirs (`cargo clean` in dev checkouts)";

/// How full the filesystem is, in the three bands the runtime cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskLevel {
    /// Plenty of room.
    Ok,
    /// Low — worth telling an operator, not worth blocking work.
    Warn,
    /// Critically low — writes are expected to start failing.
    Fail,
}

/// Classify free disk into a [`DiskLevel`]. Pure so the boundaries are
/// unit-testable without touching a real filesystem. WARN below 10% free
/// *or* below [`DISK_WARN_BYTES`] (whichever trips first); FAIL below 3%
/// free *or* below [`DISK_FAIL_BYTES`]. Percent is compared with integer
/// (u128) math to stay clippy-clean (no float casts) and overflow-free:
/// `free/total < n%`  ⇔  `free*100 < total*n`.
#[must_use]
pub fn level(free_bytes: u64, total_bytes: u64) -> DiskLevel {
    let free = u128::from(free_bytes);
    let total = u128::from(total_bytes);
    let below_pct = |n: u128| total != 0 && free * 100 < total * n;
    if below_pct(3) || free_bytes < DISK_FAIL_BYTES {
        DiskLevel::Fail
    } else if below_pct(10) || free_bytes < DISK_WARN_BYTES {
        DiskLevel::Warn
    } else {
        DiskLevel::Ok
    }
}

/// Human-readable byte size (binary units) for disk detail lines.
/// Integer-only (no float casts) to stay clippy-pedantic-clean; one
/// decimal place via scaled u128 arithmetic with round-to-nearest.
#[must_use]
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut scale: u64 = 1;
    while unit < UNITS.len() - 1 && n / scale >= 1024 {
        scale *= 1024;
        unit += 1;
    }
    if unit == 0 {
        return format!("{n} B");
    }
    let scale = u128::from(scale);
    let tenths = (u128::from(n) * 10 + scale / 2) / scale;
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

/// Integer free-space percentage (rounded down) for detail lines. Result
/// is 0..=100, so the `try_from` fallback is unreachable in practice; it
/// exists to dodge a lossy-cast clippy lint without an `as` cast.
#[must_use]
pub fn free_pct(free_bytes: u64, total_bytes: u64) -> u64 {
    if total_bytes == 0 {
        return 0;
    }
    let pct = (u128::from(free_bytes) * 100) / u128::from(total_bytes);
    u64::try_from(pct).unwrap_or(100)
}

/// `statvfs` `path` and return `(free_bytes, total_bytes)`.
///
/// `f_bavail` (not `f_bfree`) is the space available to unprivileged
/// writers — it excludes the root-reserved blocks, so it is the number
/// that actually predicts whether the next write fails with `ENOSPC`.
///
/// # Errors
///
/// Returns the raw `statvfs` errno when the path cannot be stat'd (it
/// does not exist, is not readable, ...). Callers degrade rather than
/// panic: `doctor` renders a WARN row, the host's preflight treats an
/// unknown filesystem as "not proven full" and allows the spawn.
pub fn statvfs_bytes(path: &std::path::Path) -> Result<(u64, u64), rustix::io::Errno> {
    let vfs = rustix::fs::statvfs(path)?;
    // Bytes = block count * fragment size.
    let frag = vfs.f_frsize;
    let total = vfs.f_blocks.saturating_mul(frag);
    let free = vfs.f_bavail.saturating_mul(frag);
    Ok((free, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_bands_match_the_documented_thresholds() {
        assert_eq!(level(500 * GIB, 1024 * GIB), DiskLevel::Ok);
        // 8% free, absolute fine → WARN via the percent rule.
        assert_eq!(level(80 * GIB, 1000 * GIB), DiskLevel::Warn);
        // 50% free but under the 20 GiB floor → WARN via the absolute rule.
        assert_eq!(level(15 * GIB, 30 * GIB), DiskLevel::Warn);
        // 2% free → FAIL via the percent rule.
        assert_eq!(level(20 * GIB, 1000 * GIB), DiskLevel::Fail);
        // Under the 5 GiB floor → FAIL via the absolute rule.
        assert_eq!(level(4 * GIB, 100 * GIB), DiskLevel::Fail);
        // Degenerate stat (total 0) must not divide by zero.
        assert_eq!(level(0, 0), DiskLevel::Fail);
    }

    #[test]
    fn statvfs_bytes_errors_instead_of_panicking_on_a_missing_path() {
        let missing = std::path::Path::new("/copperclaw/definitely/not/here");
        assert!(statvfs_bytes(missing).is_err());
    }
}
