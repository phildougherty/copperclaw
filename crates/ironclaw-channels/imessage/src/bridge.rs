//! Abstract transport for talking to the local macOS Messages.app + chat
//! database.
//!
//! Real production builds use [`OsaScriptBridge`](crate::bridge_osascript::OsaScriptBridge)
//! on macOS (which shells out to `osascript` for sends and `sqlite3` for
//! polls). Tests use [`MockBridge`] so they never touch the real binaries
//! or the real chat database — every behaviour the adapter relies on is
//! mediated through this trait.

use async_trait::async_trait;
use ironclaw_channels_core::AdapterError;
use std::sync::Mutex;

/// A single inbound row from `~/Library/Messages/chat.db`.
///
/// Field names are deliberately close to the underlying SQLite columns so
/// fixtures read naturally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockMessageRow {
    /// SQLite `ROWID` of the row — used as the inbound high-water mark.
    pub rowid: i64,
    /// `message.guid` — the platform-side stable id for the message.
    pub guid: String,
    /// `message.text`. `None` when the column was NULL (Messages.app
    /// occasionally stores rich content with the text body elsewhere).
    pub text: Option<String>,
    /// Raw `message.date` value — Cocoa-epoch nanoseconds or seconds, see
    /// [`crate::parse::cocoa_to_utc`].
    pub date: i64,
    /// `message.is_from_me`. The poll loop filters `is_from_me = 0` in SQL
    /// already, but we re-check at the row level too.
    pub is_from_me: bool,
    /// `handle.id` value joined via `message.handle_id`. `None` for
    /// outbound and system events.
    pub handle: Option<String>,
    /// `chat.chat_identifier` — `None` for 1:1 chats.
    pub chat_id: Option<String>,
}

/// Subprocess abstraction for the iMessage channel.
///
/// The trait carries the two operations the adapter needs:
///
/// - `run_applescript` — used by the outbound path to send messages and
///   files via `osascript -e <script>`. The argument is the full
///   AppleScript document, already escaped via
///   [`crate::applescript::applescript_escape`].
/// - `query_new_rows` — used by the inbound poll loop to fetch any rows
///   past the persisted high-water `ROWID`.
///
/// Implementations must be `Send + Sync` so they can be shared across the
/// poll task and the outbound delivery path.
#[async_trait]
pub trait IMessageBridge: Send + Sync {
    /// Execute a fully-formed AppleScript document via `osascript`.
    ///
    /// Returns the program's `stdout` on success (mostly empty for our
    /// templates; included so the bridge can be reused for read-back style
    /// scripts during testing).
    async fn run_applescript(&self, script: &str) -> Result<String, AdapterError>;

    /// Fetch every row from `message` with `ROWID > since_rowid` and
    /// `is_from_me = 0`, in `ROWID` ascending order.
    async fn query_new_rows(
        &self,
        since_rowid: i64,
    ) -> Result<Vec<MockMessageRow>, AdapterError>;
}

/// In-memory test bridge for unit tests.
///
/// The mock supports two parallel scripts:
///
/// - **AppleScript responses** — `push_applescript_ok(...)` /
///   `push_applescript_err(...)`. Each `run_applescript` call pops the next
///   scripted response, or — when the script is exhausted — replays the
///   most recent one.
/// - **Chat rows** — `set_rows(...)` configures the canonical "what's in
///   chat.db" view. `query_new_rows(since)` returns the subset of
///   currently-installed rows with `rowid > since`. The mock also has a
///   `push_query_err(...)` knob for tests that need to assert how the
///   adapter copes with sqlite3 failures.
#[derive(Default)]
pub struct MockBridge {
    state: Mutex<MockState>,
}

#[derive(Default)]
struct MockState {
    applescript_calls: Vec<String>,
    applescript_responses: Vec<Result<String, AdapterError>>,
    applescript_cursor: usize,
    rows: Vec<MockMessageRow>,
    query_errors: Vec<AdapterError>,
    query_calls: Vec<i64>,
}

impl std::fmt::Debug for MockBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBridge").finish_non_exhaustive()
    }
}

impl MockBridge {
    /// Empty mock — every call will error until something is scripted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a mock that responds to every AppleScript call with `reply`.
    pub fn always_applescript_ok(reply: impl Into<String>) -> Self {
        let mock = Self::default();
        mock.push_applescript_ok(reply);
        mock
    }

    /// Queue an OK response for the next AppleScript call.
    pub fn push_applescript_ok(&self, reply: impl Into<String>) {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.applescript_responses.push(Ok(reply.into()));
    }

    /// Queue an error response for the next AppleScript call.
    pub fn push_applescript_err(&self, err: AdapterError) {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.applescript_responses.push(Err(err));
    }

    /// Snapshot of every AppleScript document the bridge was asked to run,
    /// oldest first.
    pub fn applescript_calls(&self) -> Vec<String> {
        let g = self.state.lock().expect("mock mutex poisoned");
        g.applescript_calls.clone()
    }

    /// Number of AppleScript calls observed.
    pub fn applescript_call_count(&self) -> usize {
        let g = self.state.lock().expect("mock mutex poisoned");
        g.applescript_calls.len()
    }

    /// Replace the canonical chat.db row set the mock will serve.
    pub fn set_rows(&self, rows: Vec<MockMessageRow>) {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.rows = rows;
    }

    /// Append rows to the canonical set.
    pub fn push_rows(&self, mut rows: Vec<MockMessageRow>) {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.rows.append(&mut rows);
    }

    /// Queue an error for the next `query_new_rows` call. Errors are
    /// drained first; if no errors are queued the rows-based path runs.
    pub fn push_query_err(&self, err: AdapterError) {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.query_errors.push(err);
    }

    /// Every `since_rowid` value the mock was queried with, in order.
    pub fn query_calls(&self) -> Vec<i64> {
        let g = self.state.lock().expect("mock mutex poisoned");
        g.query_calls.clone()
    }

    /// Number of `query_new_rows` calls observed.
    pub fn query_call_count(&self) -> usize {
        let g = self.state.lock().expect("mock mutex poisoned");
        g.query_calls.len()
    }
}

#[async_trait]
impl IMessageBridge for MockBridge {
    async fn run_applescript(&self, script: &str) -> Result<String, AdapterError> {
        let result = {
            let mut g = self.state.lock().expect("mock mutex poisoned");
            g.applescript_calls.push(script.to_owned());
            if g.applescript_responses.is_empty() {
                return Err(AdapterError::Transport(
                    "mock bridge: no AppleScript response scripted".into(),
                ));
            }
            let idx = g.applescript_cursor.min(g.applescript_responses.len() - 1);
            g.applescript_cursor =
                (g.applescript_cursor + 1).min(g.applescript_responses.len());
            match &g.applescript_responses[idx] {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(clone_adapter_error(e)),
            }
        };
        result
    }

    async fn query_new_rows(
        &self,
        since_rowid: i64,
    ) -> Result<Vec<MockMessageRow>, AdapterError> {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.query_calls.push(since_rowid);
        if !g.query_errors.is_empty() {
            return Err(g.query_errors.remove(0));
        }
        let mut out: Vec<MockMessageRow> = g
            .rows
            .iter()
            .filter(|r| r.rowid > since_rowid && !r.is_from_me)
            .cloned()
            .collect();
        out.sort_by_key(|r| r.rowid);
        Ok(out)
    }
}

/// Produce a faithful copy of an `AdapterError`. `AdapterError` is not
/// `Clone`, so we re-string each variant.
pub(crate) fn clone_adapter_error(err: &AdapterError) -> AdapterError {
    match err {
        AdapterError::Io(e) => AdapterError::Transport(format!("io: {e}")),
        AdapterError::Transport(s) => AdapterError::Transport(s.clone()),
        AdapterError::Auth(s) => AdapterError::Auth(s.clone()),
        AdapterError::Rate { retry_after } => AdapterError::Rate {
            retry_after: *retry_after,
        },
        AdapterError::BadRequest(s) => AdapterError::BadRequest(s.clone()),
        AdapterError::NotImplemented => AdapterError::NotImplemented,
        AdapterError::Unsupported(s) => AdapterError::Unsupported(s.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(rowid: i64, is_from_me: bool, handle: Option<&str>) -> MockMessageRow {
        MockMessageRow {
            rowid,
            guid: format!("g-{rowid}"),
            text: Some(format!("t-{rowid}")),
            date: 0,
            is_from_me,
            handle: handle.map(str::to_owned),
            chat_id: None,
        }
    }

    #[tokio::test]
    async fn mock_no_response_errors_with_transport() {
        let m = MockBridge::new();
        let err = m.run_applescript("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn mock_records_applescript_call_log() {
        let m = MockBridge::new();
        m.push_applescript_ok("");
        m.run_applescript("(s)").await.unwrap();
        let calls = m.applescript_calls();
        assert_eq!(calls, vec!["(s)".to_string()]);
        assert_eq!(m.applescript_call_count(), 1);
    }

    #[tokio::test]
    async fn mock_replays_last_response_when_exhausted() {
        let m = MockBridge::always_applescript_ok("ok");
        assert_eq!(m.run_applescript("a").await.unwrap(), "ok");
        assert_eq!(m.run_applescript("b").await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn mock_can_yield_error() {
        let m = MockBridge::new();
        m.push_applescript_err(AdapterError::Auth("nope".into()));
        let err = m.run_applescript("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn mock_query_returns_only_new_rows() {
        let m = MockBridge::new();
        m.set_rows(vec![
            row(1, false, Some("+1")),
            row(2, false, Some("+2")),
            row(3, false, Some("+3")),
        ]);
        let rows = m.query_new_rows(1).await.unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.rowid).collect();
        assert_eq!(ids, vec![2, 3]);
    }

    #[tokio::test]
    async fn mock_query_skips_from_me() {
        let m = MockBridge::new();
        m.set_rows(vec![
            row(1, false, Some("+1")),
            row(2, true, Some("+2")),
            row(3, false, Some("+3")),
        ]);
        let rows = m.query_new_rows(0).await.unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.rowid).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[tokio::test]
    async fn mock_query_sorts_ascending() {
        let m = MockBridge::new();
        m.set_rows(vec![
            row(5, false, Some("+5")),
            row(2, false, Some("+2")),
            row(8, false, Some("+8")),
        ]);
        let rows = m.query_new_rows(0).await.unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.rowid).collect();
        assert_eq!(ids, vec![2, 5, 8]);
    }

    #[tokio::test]
    async fn mock_query_records_since_arg() {
        let m = MockBridge::new();
        m.query_new_rows(7).await.unwrap();
        m.query_new_rows(8).await.unwrap();
        assert_eq!(m.query_calls(), vec![7, 8]);
        assert_eq!(m.query_call_count(), 2);
    }

    #[tokio::test]
    async fn mock_query_yields_queued_error_first() {
        let m = MockBridge::new();
        m.set_rows(vec![row(1, false, Some("+1"))]);
        m.push_query_err(AdapterError::Transport("boom".into()));
        let err = m.query_new_rows(0).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
        // Next call should fall back to the rows-based path.
        let rows = m.query_new_rows(0).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn mock_query_empty_when_no_rows() {
        let m = MockBridge::new();
        let rows = m.query_new_rows(0).await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn mock_push_rows_appends() {
        let m = MockBridge::new();
        m.set_rows(vec![row(1, false, Some("+1"))]);
        m.push_rows(vec![row(2, false, Some("+2"))]);
        let rows = m.query_new_rows(0).await.unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.rowid).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn mock_debug_renders_struct_name() {
        let m = MockBridge::new();
        assert!(format!("{m:?}").contains("MockBridge"));
    }

    #[test]
    fn mock_message_row_clone_eq() {
        let r = row(1, false, Some("+1"));
        let r2 = r.clone();
        assert_eq!(r, r2);
    }

    #[test]
    fn clone_adapter_error_covers_each_variant() {
        let cases = vec![
            AdapterError::Transport("t".into()),
            AdapterError::Auth("a".into()),
            AdapterError::Rate { retry_after: Some(7) },
            AdapterError::Rate { retry_after: None },
            AdapterError::BadRequest("b".into()),
            AdapterError::NotImplemented,
            AdapterError::Unsupported("u".into()),
            AdapterError::Io(std::io::Error::new(std::io::ErrorKind::Other, "x")),
        ];
        for err in &cases {
            let cloned = clone_adapter_error(err);
            let _ = format!("{cloned}");
        }
    }
}
