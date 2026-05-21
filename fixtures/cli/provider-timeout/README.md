## cli / provider-timeout

Simulates the upstream LLM hanging past the runner's per-call deadline.
The runner retries up to `MAX_PROVIDER_ATTEMPTS = 3` times (each
attempt wrapped in `tokio::time::timeout(provider_deadline, ...)`),
exhausts the retry budget, marks the inbound `failed`, and emits no
chat outbound.

The wiremock plan lists three `kind=timeout` responses (one per
attempt). Each `delay_ms` exceeds the harness-side deadline so every
attempt trips. In production the deadline is sourced from
`IRONCLAW_RUNNER_PROVIDER_DEADLINE_MS` (default 60s, range 30-300s);
the replay harness sets a much smaller value so the fixture finishes
in well under a second.
