## cli / provider-5xx-retry

Simulates the upstream LLM returning HTTP 503 on the first call then
succeeding on the second. The runner backs off, retries, and delivers
the success-path reply.

The retry loop lives in `crates/copperclaw-runner/src/run.rs::query_with_retry`:
up to `MAX_PROVIDER_ATTEMPTS = 3` attempts with exponential backoff
(250ms → 500ms → 1s) honouring `ProviderError::is_retryable()`.
4xx responses fail-fast on the first attempt; 5xx, transport, overload,
and per-call deadlines all retry.
