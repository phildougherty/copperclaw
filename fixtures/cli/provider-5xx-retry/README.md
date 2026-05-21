## cli / provider-5xx-retry

Simulates the upstream LLM returning HTTP 503 on the first call then
succeeding on the second. The runner is expected to back off, retry,
and deliver the success-path reply.

Gap: as of M11 the runner has no retry loop at the `provider.query()`
level — a 5xx makes `run_loop` return Err. This fixture is `#[ignore]`d
in `replay.rs` until that retry is added; the data here pins the
post-retry shape so flipping the gate is one un-ignore away.
