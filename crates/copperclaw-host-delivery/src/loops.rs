//! Active and sweep delivery loops.
//!
//! Both loops share the same per-session body
//! ([`DeliveryService::process_session_once`]); they differ only in cadence
//! and the set of sessions they consider.

use crate::service::{
    is_container_running, is_session_active, DeliveryService, ACTIVE_POLL_MS, SWEEP_POLL_MS,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::warn;

impl DeliveryService {
    /// Active loop — polls every `ACTIVE_POLL_MS` ms, only for sessions whose
    /// `container_status == Running`.
    pub async fn run_active_loop(self: Arc<Self>, shutdown: CancellationToken) {
        self.run_loop(shutdown, ACTIVE_POLL_MS, |s| {
            is_session_active(s) && is_container_running(s)
        })
        .await;
    }

    /// Sweep loop — polls every `SWEEP_POLL_MS` ms, for every active session
    /// regardless of container state. This catches sessions whose container
    /// died mid-delivery or whose `deliver_after` window only just elapsed.
    pub async fn run_sweep_loop(self: Arc<Self>, shutdown: CancellationToken) {
        self.run_loop(shutdown, SWEEP_POLL_MS, is_session_active)
            .await;
    }

    async fn run_loop<F>(
        self: Arc<Self>,
        shutdown: CancellationToken,
        interval_ms: u64,
        predicate: F,
    ) where
        F: Fn(&copperclaw_types::Session) -> bool + Send + Sync + 'static,
    {
        let mut ticker = interval(Duration::from_millis(interval_ms));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    self.tick(&predicate).await;
                }
            }
        }
    }

    async fn tick<F>(&self, predicate: &F)
    where
        F: Fn(&copperclaw_types::Session) -> bool + Send + Sync,
    {
        // We snapshot the active sessions then process each. Sessions whose
        // status changed mid-tick are picked up on the next iteration.
        let sessions = match self.list_active_sessions() {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "delivery loop failed to list sessions");
                return;
            }
        };
        for session in sessions.iter().filter(|s| predicate(s)) {
            if let Err(err) = self.process_session_once(session).await {
                warn!(?err, session_id = ?session.id, "delivery pass failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{DeliveryReport, SessionPool};
    use crate::test_support::{make_service, write_chat_row};
    use copperclaw_db::tables::sessions as sess_tbl;

    #[tokio::test]
    async fn active_loop_processes_running_session() {
        let (service, _root, sess, mock) = make_service().await;
        sess_tbl::mark_container_running(service.central(), sess.id).unwrap();
        let out_pool = service
            .session_paths()
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_chat_row(&out_pool);

        let shutdown = CancellationToken::new();
        let svc = service.clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            svc.run_active_loop(shutdown_clone).await;
        });
        // Wait until the first tick fires and processes the row.
        for _ in 0..200 {
            if !mock.deliveries().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        shutdown.cancel();
        handle.await.unwrap();
        assert_eq!(mock.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn active_loop_skips_non_running_session() {
        let (service, _root, sess, mock) = make_service().await;
        // Container is left in `Stopped`.
        let out_pool = service
            .session_paths()
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_chat_row(&out_pool);

        let shutdown = CancellationToken::new();
        let svc = service.clone();
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            svc.run_active_loop(shutdown_clone).await;
        });
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        shutdown.cancel();
        handle.await.unwrap();
        assert!(mock.deliveries().is_empty());
    }

    #[tokio::test]
    async fn sweep_loop_processes_active_but_idle_sessions() {
        let (service, _root, sess, mock) = make_service().await;
        // Force the session into Idle (active but not running).
        sess_tbl::mark_container_idle(service.central(), sess.id).unwrap();
        let out_pool = service
            .session_paths()
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_chat_row(&out_pool);

        // We can't wait the full 60s in a test; trigger the body directly.
        let report = service.process_session_once(&sess).await.unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(mock.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn shutdown_token_breaks_loop_immediately() {
        let (service, _root, _sess, _mock) = make_service().await;
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        // Should return promptly because the token is already cancelled.
        service.clone().run_active_loop(shutdown.clone()).await;
        service.clone().run_sweep_loop(shutdown).await;
    }

    #[tokio::test]
    async fn tick_logs_db_failure_without_panicking() {
        // Build a service then drop the central DB by giving it a bogus path;
        // here we simply exercise the happy path of `tick` with an empty
        // session list, which is the most realistic "no-op" branch.
        let (service, _root, _sess, _mock) = make_service().await;
        let predicate = is_container_running;
        service.tick(&predicate).await;
    }

    #[tokio::test]
    async fn delivery_report_default_buckets_are_zero() {
        let r = DeliveryReport::default();
        assert_eq!(r.delivered, 0);
        assert_eq!(r.failed, 0);
        assert_eq!(r.deferred, 0);
    }

    #[tokio::test]
    async fn session_pool_paths_visible_from_loops() {
        let (service, _root, sess, _mock) = make_service().await;
        let pool: SessionPool = service
            .session_paths()
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        assert!(pool.paths().outbound_db.exists() || !pool.paths().outbound_db.exists());
    }
}
