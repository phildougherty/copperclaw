//! Cross-crate integration tests for the subprocess providers. Linux-only;
//! the workspace dev environment is Linux per the M9 brief.

#![cfg(unix)]

use std::path::PathBuf;

use copperclaw_providers::{
    AgentProvider, CodexProvider, OpenCodeProvider, ProviderError, QueryInput, SubprocessConfig,
    SubprocessProvider,
};
use copperclaw_types::ProviderEvent;

fn sh_args(script: &str) -> Vec<String> {
    vec!["-c".into(), script.into()]
}

#[tokio::test]
async fn codex_provider_runs_canned_session() {
    let script = "cat > /dev/null; \
         printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"itest_cx\"}'; \
         printf '%s\\n' '{\"type\":\"progress\",\"message\":\"working\"}'; \
         printf '%s\\n' '{\"type\":\"result\",\"text\":\"done\"}'";
    let p = CodexProvider::new(PathBuf::from("/bin/sh"), sh_args(script));
    let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
    q.end().await.unwrap();
    let init = q.next_event().await.unwrap();
    assert!(matches!(init, ProviderEvent::Init { .. }));
    let mut saw_progress = false;
    let mut saw_result = false;
    while let Some(ev) = q.next_event().await {
        match ev {
            ProviderEvent::Progress { message } => {
                assert_eq!(message, "working");
                saw_progress = true;
            }
            ProviderEvent::Result { text } => {
                assert_eq!(text.as_deref(), Some("done"));
                saw_result = true;
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(saw_progress && saw_result);
}

#[tokio::test]
async fn opencode_provider_accepts_push() {
    let script = "read q; read p; \
         printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"itest_oc\"}'; \
         printf '%s\\n' '{\"type\":\"result\",\"text\":\"pushed\"}'";
    let p = OpenCodeProvider::new(PathBuf::from("/bin/sh"), sh_args(script));
    let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
    q.push("hi".into()).await.unwrap();
    q.end().await.unwrap();
    let mut saw = false;
    while let Some(ev) = q.next_event().await {
        if let ProviderEvent::Result { text } = ev {
            assert_eq!(text.as_deref(), Some("pushed"));
            saw = true;
            break;
        }
    }
    assert!(saw);
}

#[tokio::test]
async fn generic_subprocess_spawn_failure_maps_to_transport() {
    let p =
        SubprocessProvider::new(SubprocessConfig::new("missing", "/no/such/binary/here"));
    let r = p.query(QueryInput::new("s", "m")).await;
    match r {
        Err(ProviderError::Transport(msg)) => assert!(msg.starts_with("spawn ")),
        Ok(_) => panic!("expected transport err"),
        Err(other) => panic!("expected transport, got {other:?}"),
    }
}

#[tokio::test]
async fn generic_subprocess_emits_decode_error_for_garbage() {
    let script = "cat > /dev/null; printf '%s\\n' 'not-json-at-all'";
    let p = SubprocessProvider::new(
        SubprocessConfig::new("sh", PathBuf::from("/bin/sh")).with_args(sh_args(script)),
    );
    let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
    q.end().await.unwrap();
    let ev = q.next_event().await.unwrap();
    match ev {
        ProviderEvent::Error { message, retryable } => {
            assert!(message.starts_with("decode: "));
            assert!(!retryable);
        }
        other => panic!("expected decode error, got {other:?}"),
    }
}
