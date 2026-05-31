# telegram/rate-limited-retry

Pins the slice-1 behaviour where `DeliveryService::bump_retry` honours
the adapter's `Rate { retry_after }` hint instead of falling back to
the default 5 s exponential backoff schedule
(see `CHANGELOG.md` [Unreleased] — "Honour adapter `Rate { retry_after }`
hints").

## What runs

1. Single telegram inbound text message.
2. Claude turn streams a one-line reply: `"Hi after rate-limit retry"`.
3. The harness has queued a `Rate { retry_after: 1 }` failure on the
   telegram adapter via the manifest's `pre_delivery_failures`.
4. First `process_session_once` call: the runner row reaches the
   adapter, `MockAdapter` pops the queued Rate error, the row is
   deferred. No `delivered` entry is recorded.
5. Manifest's `redrive_after_ms: 1200` causes the harness to sleep
   1.2 s — slightly past the 1 s `retry_after` window — and call
   `process_session_once` again.
6. Second pass: the adapter's queue is empty so `deliver` succeeds; the
   row is recorded as delivered.

## What the fixture asserts

- `expected/messages-out.jsonl` has exactly the usage row + one chat
  row. The row was NOT marked failed when the first deliver returned
  `Rate { … }` — the slice-1 contract is "defer + retry" not
  "fail-fast".
- `expected/delivered.jsonl` has exactly ONE entry, recording the
  successful second attempt (the failed first attempt does not surface
  as a `MockAdapter` delivery, only the second one does).
- The total elapsed wall time for the run is at least 1 s (`redrive_after_ms`
  is 1200 ms), implicitly pinning that the retry window was honoured —
  if the harness ignored `retry_after` and used the default 5 s
  exponential schedule, the second tick would still defer and the
  `delivered.jsonl` length would be zero, producing a diff mismatch.

## What this fixture does NOT pin

A direct numeric assertion on the backoff window's exact value (e.g.
"the retries map says `not_before = now + 1.0 s ± 50 ms`"). The
`retries` map on `DeliveryService` is private and there is no public
accessor; the closest the harness can do today is observe second-tick
behaviour at known sleep offsets. See the parent-agent's gap notes for
the API addition that would let a fixture pin the exact window length.
