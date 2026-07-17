//! `host.status` — per-loop liveness + restart counts from the M21 S1
//! background-loop supervisor (`crate::supervisor`).
//!
//! Read-only. O1 (`cclaw doctor`) consumes this to detect a dead or
//! degraded background loop; until that card lands the handler is still a
//! complete, callable surface (any socket client can issue the command).
//!
//! Response shape:
//!
//! ```json
//! {
//!   "degraded": false,
//!   "loops": [
//!     {"name": "sweep", "alive": true, "restarts": 1,
//!      "restart_streak": 1, "degraded": false,
//!      "last_exit": "panicked: ...", "uptime_secs": 42}
//!   ]
//! }
//! ```
//!
//! When the serving process has no supervisor (e.g. the fused
//! `run_server` test entry point), the handler answers with an
//! `unavailable` error rather than fabricating an empty report — an
//! all-green empty list would read as "every loop is fine".

use crate::socket::HandlerCtx;
use copperclaw_cclaw::ErrorPayload;
use serde_json::Value;

/// Handle `host.status`.
pub fn status(_args: &Value, ctx: &HandlerCtx) -> Result<Value, ErrorPayload> {
    let Some(supervisor) = &ctx.supervisor else {
        return Err(ErrorPayload::new(
            "unavailable",
            "background-loop supervisor status is not available on this host process",
        ));
    };
    // snapshot() applies the lazy healthy-reset before reporting, so read
    // the loops first and the (possibly just-cleared) degraded flag after.
    let loops = supervisor.snapshot();
    let loops = serde_json::to_value(loops)
        .map_err(|e| ErrorPayload::new("internal", format!("serialize loop status: {e}")))?;
    Ok(serde_json::json!({
        "degraded": supervisor.degraded(),
        "loops": loops,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::central::CentralDb;

    #[test]
    fn status_without_supervisor_is_unavailable() {
        let ctx = HandlerCtx::new(CentralDb::open_in_memory().unwrap());
        let err = status(&serde_json::json!({}), &ctx).unwrap_err();
        assert_eq!(err.code, "unavailable");
    }

    #[tokio::test]
    async fn status_reports_registered_loops() {
        // A supervisor with one registered (never-started) loop still
        // reports the row — registration, not liveness, defines the set.
        let sup = crate::supervisor::Supervisor::new(tokio_util::sync::CancellationToken::new());
        let mut sup = sup;
        sup.register("idle", || {
            Box::pin(std::future::pending::<()>())
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        });
        let ctx =
            HandlerCtx::new(CentralDb::open_in_memory().unwrap()).with_supervisor(sup.status());
        let out = status(&serde_json::json!({}), &ctx).unwrap();
        assert_eq!(out["degraded"], false);
        let loops = out["loops"].as_array().unwrap();
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0]["name"], "idle");
        assert_eq!(loops[0]["alive"], false);
        assert_eq!(loops[0]["restarts"], 0);
    }
}
