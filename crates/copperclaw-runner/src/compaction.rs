//! Context compaction.
//!
//! When the input transcript approaches the model's input window the runner
//! asks the provider to summarise the oldest half of the conversation,
//! archives the pre-compaction transcript to `outbox/_compactions/<RFC3339>.md`,
//! and replaces the summarised slice with a synthetic
//! `HistoryMessage::User { content: "compact_boundary: <summary>" }` entry.
//!
//! The strategy is deliberately conservative — "summarise oldest half" is
//! the cheap, predictable shape we want for the first port. A future iter
//! may switch to a tokens-based slice; the call site only needs
//! [`compact`] so we can swap the implementation behind it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use copperclaw_providers::{AgentProvider, HistoryMessage, QueryInput};
use copperclaw_types::{Effort, ProviderEvent};

/// Default conservative window we plan against (Claude Sonnet 4.5 / 4.7).
pub const DEFAULT_INPUT_WINDOW: usize = 200_000;
/// Default safety margin in tokens before we trigger compaction. Sized to
/// absorb the static request overhead the naive `estimate_tokens` doesn't
/// count: the system prompt, tool-schema JSON, and the per-turn
/// `max_tokens` output reservation. Bumped from 8K → 16K after the
/// runner shipped a 124KB inlined-skills prompt that ate the old margin
/// in one bite. Operators on much-larger windows can shrink this back
/// down via `RunnerConfigFile.safety_margin_tokens`.
pub const DEFAULT_SAFETY_MARGIN: usize = 16_000;
/// Default per-turn output reservation. Compaction subtracts this from
/// the window in addition to `safety_margin_tokens` so the API doesn't
/// reject the request for `input + max_tokens > window`. Matches the
/// runner's default `max_tokens` so the two move together.
pub const DEFAULT_OUTPUT_RESERVE: usize = 4_096;
/// System prompt the runner sends to the provider when asking for a summary.
pub const SUMMARY_SYSTEM_PROMPT: &str = "Summarize the following conversation succinctly. Preserve any decisions, \
open questions, identifiers, and unresolved tool requests. Be terse.";

/// Tuning knobs for compaction. Defaults match the constants above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionCfg {
    /// Total input window of the target model in tokens.
    pub model_input_window: usize,
    /// How much headroom to keep below the window. Compaction fires when the
    /// estimated transcript size exceeds `window - margin - output_reserve`.
    pub safety_margin_tokens: usize,
    /// Tokens reserved for the model's response (matches the runner's
    /// per-turn `max_tokens`). Subtracted from the window so the request
    /// never violates `input + max_tokens > window` — the failure mode
    /// that surfaced as a hard 400 on Haiku 4.5 with a long transcript.
    pub output_reserve_tokens: usize,
    /// Provider model name to use for the summarisation turn.
    pub summary_model: String,
    /// Effort hint for the summarisation turn.
    pub summary_effort: Effort,
    /// Max output tokens for the summarisation turn.
    pub summary_max_tokens: u32,
    /// Where to archive pre-compaction transcripts. Typically
    /// `<session_dir>/outbox/_compactions`.
    pub archive_dir: PathBuf,
}

impl CompactionCfg {
    /// True iff `estimated_tokens` has crossed the threshold. The
    /// threshold subtracts both `safety_margin_tokens` (to cover the
    /// static request overhead the estimator doesn't count — system
    /// prompt, tool schemas, formatting) and `output_reserve_tokens`
    /// (the per-turn output budget the provider enforces as part of
    /// the total window).
    #[must_use]
    pub fn should_compact(&self, estimated_tokens: usize) -> bool {
        let threshold = self
            .model_input_window
            .saturating_sub(self.safety_margin_tokens)
            .saturating_sub(self.output_reserve_tokens);
        estimated_tokens > threshold
    }
}

/// Rough token estimate: 4 characters per token. Counts the textual payload
/// of each [`HistoryMessage`] variant; tool-use input JSON is rendered as a
/// compact string for sizing purposes.
#[must_use]
pub fn estimate_tokens(messages: &[HistoryMessage]) -> usize {
    let mut chars: usize = 0;
    for m in messages {
        chars += chars_of(m);
    }
    chars / 4
}

fn chars_of(m: &HistoryMessage) -> usize {
    match m {
        HistoryMessage::User { content } | HistoryMessage::Assistant { content } => {
            content.chars().count()
        }
        HistoryMessage::ToolUse { id, name, input } => {
            id.chars().count() + name.chars().count() + input.to_string().chars().count()
        }
        HistoryMessage::Tool {
            tool_use_id,
            content,
            is_error: _,
        } => tool_use_id.chars().count() + content.chars().count(),
        HistoryMessage::Image {
            media_type,
            data: _,
        } => {
            // An image's token cost is tile-based, not its base64 length —
            // counting `data` would massively overestimate and trigger
            // needless compaction. Use a flat ~1500-token estimate
            // (the caller divides chars by 4).
            media_type.chars().count() + 6_000
        }
    }
}

/// Choose a split point near `len/2` that never bisects a `tool_use` /
/// `tool_result` group.
///
/// The slice before the pivot (`oldest`) is sent to the provider to be
/// summarised. A slice ending on a dangling `ToolUse` — or a `newest`
/// slice starting on an orphan `Tool` result — is rejected by strict
/// providers (minimax: "tool call and result not match"), which fails
/// compaction and crash-loops the runner. Advance the midpoint forward
/// past any straddled tool group so both halves are self-contained.
fn pair_safe_pivot(history: &[HistoryMessage]) -> usize {
    let mut pivot = history.len() / 2;
    while pivot < history.len()
        && (matches!(history[pivot], HistoryMessage::Tool { .. })
            || pivot
                .checked_sub(1)
                .is_some_and(|i| matches!(history[i], HistoryMessage::ToolUse { .. })))
    {
        pivot += 1;
    }
    pivot
}

/// Replace the oldest half of `history` with a single summarised user-side
/// `compact_boundary` entry. Writes the pre-compaction transcript to
/// `cfg.archive_dir/<RFC3339>.md` as a side effect.
///
/// If `history.len() < 4` the function is a no-op and returns the input
/// unchanged — there isn't enough material to summarise meaningfully.
pub async fn compact(
    history: Vec<HistoryMessage>,
    provider: &dyn AgentProvider,
    cfg: &CompactionCfg,
) -> Result<Vec<HistoryMessage>> {
    if history.len() < 4 {
        return Ok(history);
    }
    let pivot = pair_safe_pivot(&history);
    if pivot == 0 || pivot >= history.len() {
        // The whole transcript is one unsplittable tool group (rare). Leave
        // it intact rather than send a half-pair to the provider.
        return Ok(history);
    }
    let oldest = history[..pivot].to_vec();
    let newest = history[pivot..].to_vec();

    write_archive(&cfg.archive_dir, &history)
        .with_context(|| format!("archive transcript to {}", cfg.archive_dir.display()))?;

    let summary = summarise(provider, cfg, oldest).await?;

    let mut out = Vec::with_capacity(newest.len() + 1);
    out.push(HistoryMessage::User {
        content: format!("compact_boundary: {summary}"),
    });
    out.extend(newest);
    Ok(out)
}

/// Drive one summarisation turn against the provider, collecting the final
/// [`ProviderEvent::Result`] text into a string.
async fn summarise(
    provider: &dyn AgentProvider,
    cfg: &CompactionCfg,
    oldest: Vec<HistoryMessage>,
) -> Result<String> {
    let input = QueryInput {
        system: SUMMARY_SYSTEM_PROMPT.into(),
        system_context: None,
        model: cfg.summary_model.clone(),
        effort: cfg.summary_effort,
        previous_continuation: None,
        history: oldest,
        tools: Vec::new(),
        max_tokens: cfg.summary_max_tokens,
        temperature: Some(0.0),
        assistant_name: None,
        display_name: None,
    };
    let mut query = provider
        .query(input)
        .await
        .context("summarisation provider query failed")?;
    let mut summary = String::new();
    while let Some(event) = query.next_event().await {
        match event {
            ProviderEvent::Result { text } => {
                if let Some(t) = text {
                    summary.push_str(&t);
                }
                break;
            }
            ProviderEvent::Error { message, .. } => {
                anyhow::bail!("summarisation error from provider: {message}");
            }
            _ => continue,
        }
    }
    if summary.trim().is_empty() {
        anyhow::bail!("provider returned an empty summary");
    }
    Ok(summary)
}

fn write_archive(dir: &Path, history: &[HistoryMessage]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = Utc::now().to_rfc3339();
    let safe = stamp.replace(':', "-");
    let target = dir.join(format!("{safe}.md"));
    let mut body = String::new();
    body.push_str("# Compaction archive\n\n");
    body.push_str("Archived at ");
    body.push_str(&stamp);
    body.push_str("\n\n");
    for (i, m) in history.iter().enumerate() {
        body.push_str(&format!("## {i}. "));
        match m {
            HistoryMessage::User { content } => {
                body.push_str("user\n\n");
                body.push_str(content);
            }
            HistoryMessage::Assistant { content } => {
                body.push_str("assistant\n\n");
                body.push_str(content);
            }
            HistoryMessage::ToolUse { id, name, input } => {
                body.push_str("tool_use\n\n");
                body.push_str(&format!("id: {id}\nname: {name}\ninput: {input}"));
            }
            HistoryMessage::Tool {
                tool_use_id,
                content,
                is_error,
            } => {
                body.push_str("tool_result\n\n");
                body.push_str(&format!(
                    "tool_use_id: {tool_use_id}\nis_error: {is_error}\n\n{content}"
                ));
            }
            HistoryMessage::Image { media_type, data } => {
                // Record presence + size only; the base64 payload would
                // bloat the archive for no human benefit.
                body.push_str("image\n\n");
                body.push_str(&format!(
                    "media_type: {media_type}\nbase64_bytes: {}",
                    data.len()
                ));
            }
        }
        body.push_str("\n\n");
    }
    std::fs::write(&target, body)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use copperclaw_providers::{AgentProvider, AgentQuery, ProviderError};
    use std::sync::Mutex;

    fn cfg_with_dir(dir: PathBuf) -> CompactionCfg {
        CompactionCfg {
            model_input_window: 200_000,
            safety_margin_tokens: 16_000,
            output_reserve_tokens: 4_096,
            summary_model: "claude-sonnet-4-6".into(),
            summary_effort: Effort::Low,
            summary_max_tokens: 1024,
            archive_dir: dir,
        }
    }

    #[test]
    fn should_compact_threshold() {
        // 200_000 - 16_000 (margin) - 4_096 (output reserve) = 179_904
        let cfg = cfg_with_dir(PathBuf::from("/tmp/x"));
        assert!(!cfg.should_compact(100));
        assert!(!cfg.should_compact(179_904));
        assert!(cfg.should_compact(179_905));
        assert!(cfg.should_compact(1_000_000));
    }

    fn tu(id: &str) -> HistoryMessage {
        HistoryMessage::ToolUse {
            id: id.into(),
            name: "t".into(),
            input: serde_json::json!({}),
        }
    }
    fn tr(id: &str) -> HistoryMessage {
        HistoryMessage::Tool {
            tool_use_id: id.into(),
            content: "ok".into(),
            is_error: false,
        }
    }
    fn txt() -> HistoryMessage {
        HistoryMessage::User {
            content: "x".into(),
        }
    }

    #[test]
    fn pivot_does_not_split_a_tool_pair() {
        // Naive len/2 pivot (3) lands on the Tool result of the pair at 2..4.
        let h = vec![txt(), txt(), tu("a"), tr("a"), txt(), txt()];
        let p = pair_safe_pivot(&h);
        assert!(p >= 4, "pivot {p} must clear the tool group");
        // oldest must not end on a dangling ToolUse...
        assert!(!matches!(h[p - 1], HistoryMessage::ToolUse { .. }));
        // ...and newest must not start on an orphan Tool result.
        assert!(p >= h.len() || !matches!(h[p], HistoryMessage::Tool { .. }));
    }

    #[test]
    fn pivot_clears_a_straddled_parallel_tool_group() {
        // Parallel batch [tu a, tu b, tr a, tr b] straddles the naive
        // midpoint (4). The pivot must advance to the end of the group (6).
        let h = vec![
            txt(),
            txt(),
            tu("a"),
            tu("b"),
            tr("a"),
            tr("b"),
            txt(),
            txt(),
        ];
        let p = pair_safe_pivot(&h); // naive pivot = 4 (history[4] = tr a)
        assert_eq!(p, 6);
        assert!(!matches!(h[p - 1], HistoryMessage::ToolUse { .. }));
        assert!(!matches!(h[p], HistoryMessage::Tool { .. }));
    }

    #[test]
    fn pivot_is_plain_midpoint_when_no_pair_straddles() {
        let h = vec![txt(), txt(), txt(), txt()];
        assert_eq!(pair_safe_pivot(&h), 2);
    }

    #[test]
    fn should_compact_when_margin_exceeds_window() {
        let cfg = CompactionCfg {
            model_input_window: 100,
            safety_margin_tokens: 500,
            output_reserve_tokens: 0,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        // saturating_sub keeps threshold at 0; any positive estimate compacts.
        assert!(cfg.should_compact(1));
    }

    #[test]
    fn should_compact_accounts_for_output_reserve() {
        // Regression for the live Haiku-4.5 200K-window overflow: with
        // an 8K safety margin and a 4K output reserve, the model rejects
        // requests where `input + max_tokens > window`. The threshold
        // must subtract BOTH so the API never sees that combination.
        let cfg = CompactionCfg {
            model_input_window: 200_000,
            safety_margin_tokens: 8_000,
            output_reserve_tokens: 4_096,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        // 200_000 - 8_000 - 4_096 = 187_904
        assert!(!cfg.should_compact(187_904));
        assert!(cfg.should_compact(187_905));
        // The pre-fix bug: estimated_tokens=195_000 would NOT have
        // triggered compaction under the old `input - margin` rule
        // (195_000 < 192_000 was false — but `195_000 > 192_000` was
        // true, so OLD code did compact at this point. The new failure
        // path was around `input=190_000` with `max_tokens=4_096`:
        // old rule: 190K < 192K → no compact → 194K total → fail.
        // New rule: 190K > 187_904 → compact → safe.
        assert!(cfg.should_compact(190_000));
    }

    #[test]
    fn estimate_tokens_for_simple_text() {
        let h = vec![HistoryMessage::User {
            content: "a".repeat(8),
        }];
        assert_eq!(estimate_tokens(&h), 2);
    }

    #[test]
    fn estimate_tokens_handles_all_variants() {
        let h = vec![
            HistoryMessage::User {
                content: "abcd".into(),
            },
            HistoryMessage::Assistant {
                content: "wxyz".into(),
            },
            HistoryMessage::ToolUse {
                id: "tu_1".into(),
                name: "tool".into(),
                input: serde_json::json!({"k": "v"}),
            },
            HistoryMessage::Tool {
                tool_use_id: "tu_1".into(),
                content: "result".into(),
                is_error: false,
            },
        ];
        // Just sanity-check that we got a positive number and didn't panic.
        assert!(estimate_tokens(&h) > 0);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    /// Provider stub that returns a canned summary text.
    struct StubProvider {
        canned_summary: String,
    }

    #[async_trait]
    impl AgentProvider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            Ok(Box::new(StubQuery {
                events: Mutex::new(vec![
                    ProviderEvent::Init {
                        continuation: "c1".into(),
                    },
                    ProviderEvent::Result {
                        text: Some(self.canned_summary.clone()),
                    },
                ]),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    /// Provider stub that returns an Error event.
    struct ErrorProvider;

    #[async_trait]
    impl AgentProvider for ErrorProvider {
        fn name(&self) -> &'static str {
            "err"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            Ok(Box::new(StubQuery {
                events: Mutex::new(vec![ProviderEvent::Error {
                    message: "synthetic".into(),
                    retryable: false,
                }]),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct StubQuery {
        events: Mutex<Vec<ProviderEvent>>,
    }

    #[async_trait]
    impl AgentQuery for StubQuery {
        async fn push(&mut self, _message: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            let mut g = self.events.lock().unwrap();
            if g.is_empty() {
                None
            } else {
                Some(g.remove(0))
            }
        }
        async fn abort(&mut self) {}
    }

    fn long_history(n: usize) -> Vec<HistoryMessage> {
        (0..n)
            .map(|i| HistoryMessage::User {
                content: format!("msg-{i}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn compact_replaces_oldest_half_with_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = StubProvider {
            canned_summary: "SUMMARY".into(),
        };
        let h = long_history(8);
        let out = compact(h, &provider, &cfg).await.unwrap();
        assert_eq!(out.len(), 5);
        match &out[0] {
            HistoryMessage::User { content } => {
                assert!(content.starts_with("compact_boundary: "));
                assert!(content.contains("SUMMARY"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_archives_transcript_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = StubProvider {
            canned_summary: "S".into(),
        };
        let _ = compact(long_history(4), &provider, &cfg).await.unwrap();
        // At least one archive file landed in the directory.
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(!files.is_empty(), "expected an archive file");
        // And it has the markdown shape we wrote.
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(body.starts_with("# Compaction archive"));
    }

    #[tokio::test]
    async fn compact_noop_for_tiny_history() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = StubProvider {
            canned_summary: "S".into(),
        };
        let h = long_history(3);
        let out = compact(h.clone(), &provider, &cfg).await.unwrap();
        assert_eq!(out, h);
    }

    #[tokio::test]
    async fn compact_propagates_provider_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = ErrorProvider;
        let err = compact(long_history(8), &provider, &cfg).await.unwrap_err();
        assert!(err.to_string().contains("synthetic"));
    }

    #[tokio::test]
    async fn compact_errors_on_empty_summary() {
        struct EmptyProvider;
        #[async_trait]
        impl AgentProvider for EmptyProvider {
            fn name(&self) -> &'static str {
                "empty"
            }
            async fn query(
                &self,
                _input: QueryInput,
            ) -> Result<Box<dyn AgentQuery>, ProviderError> {
                Ok(Box::new(StubQuery {
                    events: Mutex::new(vec![ProviderEvent::Result {
                        text: Some(String::new()),
                    }]),
                }))
            }
            fn is_session_invalid(&self, _err: &ProviderError) -> bool {
                false
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let err = compact(long_history(8), &EmptyProvider, &cfg)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty summary"));
    }

    #[test]
    fn summary_system_prompt_is_non_empty() {
        // Static assertion: a runtime check on a const string is optimised
        // out, so encode the invariant in `const _: () =`.
        const _: () = assert!(!SUMMARY_SYSTEM_PROMPT.is_empty());
        // And give the test something dynamic to look at so the test is
        // visible in coverage reports.
        let copy = SUMMARY_SYSTEM_PROMPT.to_string();
        assert_eq!(copy, SUMMARY_SYSTEM_PROMPT);
    }

    #[test]
    fn default_constants_are_positive() {
        const _: () = assert!(DEFAULT_INPUT_WINDOW > DEFAULT_SAFETY_MARGIN);
        // Runtime touch so the symbols are referenced from the test binary.
        let _ = DEFAULT_INPUT_WINDOW.to_string();
        let _ = DEFAULT_SAFETY_MARGIN.to_string();
    }
}
