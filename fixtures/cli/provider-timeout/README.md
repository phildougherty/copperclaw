## cli / provider-timeout

Simulates the upstream LLM hanging past the configured per-call budget.
Expected behaviour: the runner gives up after the cap, marks the inbound
`failed`, and emits no chat outbound.

Gap: the runner has no per-`provider.query()` deadline today — only the
provider's own 600s reqwest client timeout. This fixture is `#[ignore]`d
in `replay.rs` until a runner-side deadline is added; the data pins the
post-timeout shape so flipping the gate is one un-ignore away.
