//! Live test against a real Ollama server. `#[ignore]`d by default; opt in
//! with:
//!
//! ```text
//! cargo test --ignored ollama_live -p copperclaw-providers
//! ```
//!
//! Reads `OLLAMA_HOST` (defaults to `http://localhost:11434`) and
//! `OLLAMA_MODEL` (defaults to `llama3.1:8b`). Sends a single 2+2 prompt
//! and asserts that the final [`ProviderEvent::Result`] carries non-empty
//! text. Useful as a smoke test after a runtime upgrade or when wiring a
//! new local model.
//!
//! The test fails fast (60s ceiling) so a missing model or wedged server
//! won't pin the suite.

use std::time::Duration;

use copperclaw_providers::{AgentProvider, HistoryMessage, OllamaProvider, QueryInput};
use copperclaw_types::ProviderEvent;

#[tokio::test]
#[ignore = "requires a running Ollama server; opt in with --ignored"]
async fn ollama_live_simple_prompt_round_trip() {
    let host =
        std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.1:8b".to_string());
    let p = OllamaProvider::new(host, Some(model.clone()));

    let mut input = QueryInput::new("You are a precise calculator.", model);
    input.history.push(HistoryMessage::User {
        content: "What is 2+2? Answer with just the number.".into(),
    });
    input.max_tokens = 64;

    let mut q = p.query(input).await.expect("query starts; is ollama running?");
    let mut final_text: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "ollama live test timed out (final={final_text:?})"
        );
        match tokio::time::timeout(remaining, q.next_event()).await {
            Ok(Some(ProviderEvent::Result { text })) => {
                final_text = text;
                break;
            }
            Ok(Some(ProviderEvent::Error { message, .. })) => {
                panic!("provider error: {message}");
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(elapsed) => panic!("ollama live test timed out ({elapsed})"),
        }
    }
    let text = final_text.expect("got a Result with text");
    assert!(!text.trim().is_empty(), "ollama returned empty text");
}
