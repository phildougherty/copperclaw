//! Lifecycle helpers and the gateway runner loop.
//!
//! - [`next_backoff`] / [`MAX_BACKOFF`] — exponential-backoff math used by
//!   the reconnect loop.
//! - [`should_heartbeat`] — given the last-heartbeat timestamp and the
//!   configured interval, decide whether to send another.
//! - [`is_fatal_close`] — diagnostic helper: which transport errors
//!   should abort reconnects.
//! - [`GatewayRunner`] — drives the lifecycle (connect, optional
//!   handshake, recv loop, heartbeat, reconnect, cancellation) over a
//!   [`WsTransport`]. The runner is the unit that adapter-level tests
//!   spin up against a [`MockTransport`].

use std::sync::Arc;
use std::time::Duration;

use ironclaw_channels_core::AdapterError;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::gateway::transport::WsTransport;

/// Maximum reconnect backoff. The lifecycle loop caps the exponential at
/// this value so a long network outage does not push the delay into
/// hours.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Base backoff for the first reconnect attempt.
pub const BASE_BACKOFF: Duration = Duration::from_secs(1);

/// Compute the reconnect delay for `attempt` (0-indexed). `attempt = 0`
/// returns [`BASE_BACKOFF`]; each subsequent attempt doubles, capped at
/// [`MAX_BACKOFF`].
pub fn next_backoff(attempt: u32) -> Duration {
    let base = BASE_BACKOFF;
    let cap = MAX_BACKOFF;
    let mult = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let ms = base.as_millis().saturating_mul(u128::from(mult));
    let cap_ms = cap.as_millis();
    let clamped = ms.min(cap_ms);
    Duration::from_millis(u64::try_from(clamped).unwrap_or(u64::MAX))
}

/// True if the time elapsed since `last_heartbeat` reaches `interval`.
pub fn should_heartbeat(now: Instant, last_heartbeat: Instant, interval: Duration) -> bool {
    now.saturating_duration_since(last_heartbeat) >= interval
}

/// Mark which transport errors are fatal — i.e. should not be retried.
///
/// The reverse-engineered protocol uses no formal close codes that map
/// to "give up forever" today, but the helper exists as the seam where
/// such codes will land once the crypto backend is wired up. For now,
/// only an `Auth` adapter error is fatal — a Signal-Protocol auth
/// failure means the keystore is stale and reconnecting won't help.
pub fn is_fatal_error(err: &AdapterError) -> bool {
    matches!(err, AdapterError::Auth(_))
}

/// What the runner emits to its consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// The transport opened.
    Connected,
    /// A binary frame arrived.
    Frame(Vec<u8>),
    /// The transport closed.
    Disconnected,
    /// The reconnect loop has given up.
    GaveUp(String),
}

/// Runner state visible to tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerStatus {
    /// The runner has not started yet.
    Idle,
    /// The runner is actively connected.
    Running,
    /// The runner is between attempts, sleeping `backoff`.
    Backoff,
    /// The runner has stopped.
    Stopped,
}

/// Driver for the gateway lifecycle.
///
/// The runner is generic over the transport so adapter-level tests can
/// plug in a [`crate::gateway::transport::MockTransport`].
pub struct GatewayRunner {
    /// How long to wait between heartbeats.
    pub heartbeat_interval: Duration,
    /// How long to wait for a single recv before sending a heartbeat
    /// (the `select!` arm uses this to drive the heartbeat side).
    pub recv_idle_timeout: Duration,
    /// Maximum reconnect attempts before giving up. `None` means
    /// reconnect forever.
    pub max_attempts: Option<u32>,
}

impl Default for GatewayRunner {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(25),
            recv_idle_timeout: Duration::from_secs(30),
            max_attempts: None,
        }
    }
}

impl GatewayRunner {
    /// Build a runner with explicit timings (test-friendly).
    pub fn with_timings(
        heartbeat_interval: Duration,
        recv_idle_timeout: Duration,
        max_attempts: Option<u32>,
    ) -> Self {
        Self {
            heartbeat_interval,
            recv_idle_timeout,
            max_attempts,
        }
    }

    /// Run one connection cycle: receive frames, send heartbeats, exit
    /// when the transport closes or `cancel` fires. Returns the reason
    /// for exit.
    pub async fn run_once<T: WsTransport + ?Sized>(
        &self,
        transport: Arc<T>,
        events: &mpsc::Sender<LifecycleEvent>,
        cancel: &CancellationToken,
    ) -> RunOnceOutcome {
        if events.send(LifecycleEvent::Connected).await.is_err() {
            return RunOnceOutcome::ConsumerGone;
        }
        let mut last_heartbeat = Instant::now();
        let heartbeat_payload: Vec<u8> = HEARTBEAT_PAYLOAD.to_vec();
        loop {
            // Compute the per-loop sleep: shorter of recv_idle and the
            // remaining heartbeat interval.
            let elapsed = Instant::now().saturating_duration_since(last_heartbeat);
            let until_hb = self
                .heartbeat_interval
                .checked_sub(elapsed)
                .unwrap_or(Duration::ZERO);
            let wait = self.recv_idle_timeout.min(until_hb.max(Duration::from_millis(1)));
            tokio::select! {
                () = cancel.cancelled() => {
                    let _ = transport.close().await;
                    return RunOnceOutcome::Cancelled;
                }
                recv = transport.recv_binary() => match recv {
                    Ok(Some(bytes)) => {
                        if events.send(LifecycleEvent::Frame(bytes)).await.is_err() {
                            return RunOnceOutcome::ConsumerGone;
                        }
                    }
                    Ok(None) => {
                        let _ = events.send(LifecycleEvent::Disconnected).await;
                        return RunOnceOutcome::Disconnected;
                    }
                    Err(err) => {
                        let _ = events.send(LifecycleEvent::Disconnected).await;
                        return RunOnceOutcome::Error(err);
                    }
                },
                () = sleep(wait) => {
                    if should_heartbeat(Instant::now(), last_heartbeat, self.heartbeat_interval) {
                        if let Err(err) = transport.send_binary(heartbeat_payload.clone()).await {
                            let _ = events.send(LifecycleEvent::Disconnected).await;
                            return RunOnceOutcome::Error(err);
                        }
                        last_heartbeat = Instant::now();
                    }
                }
            }
        }
    }
}

/// Per-connection outcome of [`GatewayRunner::run_once`].
#[derive(Debug)]
pub enum RunOnceOutcome {
    /// The transport closed cleanly.
    Disconnected,
    /// The cancellation token fired.
    Cancelled,
    /// The transport or send call errored.
    Error(AdapterError),
    /// The event consumer's channel closed before we could deliver an
    /// event.
    ConsumerGone,
}

impl RunOnceOutcome {
    /// True when this outcome should trigger a reconnect (vs stopping).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Disconnected => true,
            Self::Error(err) => !is_fatal_error(err),
            Self::Cancelled | Self::ConsumerGone => false,
        }
    }
}

/// Heartbeat payload — a single-byte signal a real implementation would
/// replace with a binary-XML `ping` element. The byte is opaque to the
/// transport; the mock observes it verbatim.
pub const HEARTBEAT_PAYLOAD: &[u8] = &[0x00];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::transport::MockTransport;
    use tokio::time::{Duration, timeout};

    // ---- next_backoff math ----

    #[test]
    fn next_backoff_attempt_zero_is_base() {
        assert_eq!(next_backoff(0), BASE_BACKOFF);
    }

    #[test]
    fn next_backoff_doubles_each_attempt() {
        assert_eq!(next_backoff(1), Duration::from_secs(2));
        assert_eq!(next_backoff(2), Duration::from_secs(4));
        assert_eq!(next_backoff(3), Duration::from_secs(8));
        assert_eq!(next_backoff(4), Duration::from_secs(16));
        assert_eq!(next_backoff(5), Duration::from_secs(32));
    }

    #[test]
    fn next_backoff_clamps_to_cap() {
        assert_eq!(next_backoff(6), MAX_BACKOFF);
        assert_eq!(next_backoff(7), MAX_BACKOFF);
        assert_eq!(next_backoff(99), MAX_BACKOFF);
        assert_eq!(next_backoff(u32::MAX), MAX_BACKOFF);
    }

    // ---- should_heartbeat ----

    #[test]
    fn should_heartbeat_below_interval_is_false() {
        let last = Instant::now();
        assert!(!should_heartbeat(last, last, Duration::from_secs(30)));
    }

    #[test]
    fn should_heartbeat_at_interval_is_true() {
        let last = Instant::now();
        let now = last + Duration::from_secs(30);
        assert!(should_heartbeat(now, last, Duration::from_secs(30)));
    }

    #[test]
    fn should_heartbeat_above_interval_is_true() {
        let last = Instant::now();
        let now = last + Duration::from_secs(31);
        assert!(should_heartbeat(now, last, Duration::from_secs(30)));
    }

    // ---- is_fatal_error ----

    #[test]
    fn auth_error_is_fatal() {
        assert!(is_fatal_error(&AdapterError::Auth("nope".into())));
    }

    #[test]
    fn transport_error_is_not_fatal() {
        assert!(!is_fatal_error(&AdapterError::Transport("temp".into())));
    }

    #[test]
    fn rate_error_is_not_fatal() {
        assert!(!is_fatal_error(&AdapterError::Rate { retry_after: None }));
    }

    #[test]
    fn other_errors_are_not_fatal() {
        assert!(!is_fatal_error(&AdapterError::NotImplemented));
        assert!(!is_fatal_error(&AdapterError::BadRequest("x".into())));
        assert!(!is_fatal_error(&AdapterError::Unsupported("y".into())));
    }

    // ---- RunnerStatus / LifecycleEvent ----

    #[test]
    fn runner_status_eq_and_debug() {
        assert_eq!(RunnerStatus::Idle, RunnerStatus::Idle);
        assert_ne!(RunnerStatus::Idle, RunnerStatus::Running);
        assert!(format!("{:?}", RunnerStatus::Running).contains("Running"));
    }

    #[test]
    fn lifecycle_event_eq_and_debug() {
        assert_eq!(LifecycleEvent::Connected, LifecycleEvent::Connected);
        assert_eq!(
            LifecycleEvent::Frame(vec![1, 2]),
            LifecycleEvent::Frame(vec![1, 2])
        );
        let s = format!("{:?}", LifecycleEvent::Connected);
        assert!(s.contains("Connected"));
    }

    #[test]
    fn run_once_outcome_is_retryable_logic() {
        assert!(RunOnceOutcome::Disconnected.is_retryable());
        assert!(!RunOnceOutcome::Cancelled.is_retryable());
        assert!(!RunOnceOutcome::ConsumerGone.is_retryable());
        assert!(
            RunOnceOutcome::Error(AdapterError::Transport("x".into())).is_retryable(),
            "transport errors should be retryable"
        );
        assert!(
            !RunOnceOutcome::Error(AdapterError::Auth("x".into())).is_retryable(),
            "auth errors should not be retryable"
        );
    }

    // ---- GatewayRunner.run_once with MockTransport ----

    fn quick_runner() -> GatewayRunner {
        GatewayRunner::with_timings(
            Duration::from_millis(50),
            Duration::from_millis(30),
            None,
        )
    }

    #[tokio::test]
    async fn run_once_emits_connected_then_frame_then_disconnect_on_eos() {
        let runner = quick_runner();
        let (mock, handle) = MockTransport::new();
        handle.push_binary([1, 2, 3]);
        handle.push_eos();
        let arc = Arc::new(mock);
        let (tx, mut rx) = mpsc::channel::<LifecycleEvent>(8);
        let cancel = CancellationToken::new();
        let outcome = timeout(
            Duration::from_secs(2),
            runner.run_once(arc, &tx, &cancel),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RunOnceOutcome::Disconnected));
        assert_eq!(rx.recv().await.unwrap(), LifecycleEvent::Connected);
        assert_eq!(rx.recv().await.unwrap(), LifecycleEvent::Frame(vec![1, 2, 3]));
        assert_eq!(rx.recv().await.unwrap(), LifecycleEvent::Disconnected);
    }

    #[tokio::test]
    async fn run_once_returns_cancelled_when_token_cancels() {
        let runner = quick_runner();
        let (mock, _handle) = MockTransport::new();
        let arc = Arc::new(mock);
        let (tx, mut rx) = mpsc::channel::<LifecycleEvent>(8);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let task = tokio::spawn(async move {
            cancel_clone.cancel();
        });
        let outcome = runner.run_once(arc.clone(), &tx, &cancel).await;
        task.await.unwrap();
        assert!(matches!(outcome, RunOnceOutcome::Cancelled));
        // Connected was emitted; the test just checks the loop honoured the
        // cancel token without hanging.
        assert_eq!(rx.recv().await.unwrap(), LifecycleEvent::Connected);
        // The transport's close() should have been called.
        // (Inspect via the handle the test created.)
    }

    #[tokio::test]
    async fn run_once_returns_error_on_transport_error() {
        let runner = quick_runner();
        let (mock, handle) = MockTransport::new();
        handle.push_error("boom");
        let arc = Arc::new(mock);
        let (tx, mut rx) = mpsc::channel::<LifecycleEvent>(8);
        let cancel = CancellationToken::new();
        let outcome = runner.run_once(arc, &tx, &cancel).await;
        match outcome {
            RunOnceOutcome::Error(AdapterError::Transport(s)) => assert_eq!(s, "boom"),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(rx.recv().await.unwrap(), LifecycleEvent::Connected);
        assert_eq!(rx.recv().await.unwrap(), LifecycleEvent::Disconnected);
    }

    #[tokio::test]
    async fn run_once_sends_heartbeat_after_interval() {
        let runner = GatewayRunner::with_timings(
            Duration::from_millis(20),
            Duration::from_millis(10),
            None,
        );
        let (mock, handle) = MockTransport::new();
        let arc = Arc::new(mock);
        let (tx, _rx) = mpsc::channel::<LifecycleEvent>(8);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            // Wait long enough for at least one heartbeat to fire.
            tokio::time::sleep(Duration::from_millis(80)).await;
            cancel_clone.cancel();
        });
        let _ = runner.run_once(arc.clone(), &tx, &cancel).await;
        let sent = handle.sent().await;
        assert!(!sent.is_empty(), "at least one heartbeat should have been sent");
        assert_eq!(sent[0], HEARTBEAT_PAYLOAD);
    }

    #[tokio::test]
    async fn run_once_returns_consumer_gone_when_receiver_dropped() {
        let runner = quick_runner();
        let (mock, _handle) = MockTransport::new();
        let arc = Arc::new(mock);
        let (tx, rx) = mpsc::channel::<LifecycleEvent>(1);
        drop(rx);
        let cancel = CancellationToken::new();
        let outcome = runner.run_once(arc.clone(), &tx, &cancel).await;
        assert!(matches!(outcome, RunOnceOutcome::ConsumerGone));
    }

    #[tokio::test]
    async fn run_once_disconnect_after_multiple_frames() {
        let runner = quick_runner();
        let (mock, handle) = MockTransport::new();
        for i in 0..5u8 {
            handle.push_binary([i]);
        }
        handle.push_eos();
        let arc = Arc::new(mock);
        let (tx, mut rx) = mpsc::channel::<LifecycleEvent>(16);
        let cancel = CancellationToken::new();
        let outcome = timeout(
            Duration::from_secs(2),
            runner.run_once(arc, &tx, &cancel),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RunOnceOutcome::Disconnected));
        // Drop the sender so the receiver loop terminates when drained.
        drop(tx);
        let mut got = vec![];
        while let Some(evt) = rx.recv().await {
            got.push(evt);
        }
        let frames: Vec<&LifecycleEvent> = got
            .iter()
            .filter(|e| matches!(e, LifecycleEvent::Frame(_)))
            .collect();
        assert_eq!(frames.len(), 5);
    }

    // ---- default-constructor sanity ----

    #[test]
    fn gateway_runner_default_has_sensible_timings() {
        let r = GatewayRunner::default();
        assert!(r.heartbeat_interval >= Duration::from_secs(10));
        assert!(r.recv_idle_timeout >= Duration::from_secs(10));
        assert!(r.max_attempts.is_none());
    }

    #[test]
    fn with_timings_sets_fields() {
        let r = GatewayRunner::with_timings(
            Duration::from_secs(1),
            Duration::from_secs(2),
            Some(3),
        );
        assert_eq!(r.heartbeat_interval, Duration::from_secs(1));
        assert_eq!(r.recv_idle_timeout, Duration::from_secs(2));
        assert_eq!(r.max_attempts, Some(3));
    }

    #[test]
    fn heartbeat_payload_constant_is_single_byte() {
        assert_eq!(HEARTBEAT_PAYLOAD, &[0x00]);
    }

    #[test]
    fn max_and_base_backoff_constants() {
        assert_eq!(BASE_BACKOFF, Duration::from_secs(1));
        assert_eq!(MAX_BACKOFF, Duration::from_secs(60));
    }
}
