# cli/delivery-retry-restart

M21 S3 (Wave-1 X-rider): delivery retry counters survive a service
restart, with exactly-once dead-lettering — pinned at the replay level
against a runner-produced outbound row, using the harness's
`ReplayHarness::restart_delivery` seam (the harness extension the S3
card deferred to the X-rider). Registered as
`cli_delivery_retry_restart_resumes_and_dead_letters_once` in
`crates/copperclaw-host/tests/replay.rs`.

## What runs

1. One inbound turn ("say hi" -> "Hi across the restart!") produces a
   real chat row through the full pipeline. The manifest queues one
   `transport` failure on the cli adapter (`pre_delivery_failures`), so
   the fixture-run delivery pass fails attempt 1 and the expected
   `delivered.jsonl` is EMPTY — while migration 029 persists `tries = 1`
   plus a wall-clock `not_before` window on the `messages_out` row.
2. The test then restarts the delivery service BEFORE EACH subsequent
   attempt (`restart_delivery()`: fresh `DeliveryService` — empty
   in-memory retry cache, primed-set, in-flight guards — plus fresh
   `MockAdapter`s over the same central DB and per-session files). Every
   attempt is therefore served by a service that must re-prime its
   retry state from the row.
3. Attempts 2 and 3 are scripted transport failures. Three failures
   across three service lifetimes exhaust `MAX_DELIVERY_ATTEMPTS` (3):
   if any restart reset the budget, the third attempt would defer
   instead of dead-lettering.
4. Exhaustion dead-letters exactly once: one terminal
   `delivered{status="failed"}` record and one delivery-failure
   ErrorCard row. (Retry exhaustion does not feed the central
   `outbound_dropped_messages` table — that is the S5 no-adapter expiry
   path.)
5. A fourth service incarnation delivers the ErrorCard to the wire
   exactly once ("Could not deliver message"), then a further pass — and
   the terminal records — change nothing. The poisoned chat text itself
   never reaches any adapter incarnation.

## Backoff windows

Between attempts the test rewinds the PERSISTED `not_before` into the
past ("the host was down longer than the backoff window") via the public
`messages_out::set_retry_state`. That is legitimate here because each
restarted service reads the window from the row — the exact S3 contract
under test; real sleeps would pin the same thing slowly and jittery. The
in-window deferral behavior itself (a live service honoring its backoff)
is pinned by `telegram/rate-limited-retry` and the delivery crate's unit
tests.

## Relation to the S3 crate-level tests

`crates/copperclaw-host-delivery/src/service.rs` already pins these
semantics unit-style (`restart_resumes_persisted_attempt_count`,
`persisted_exhaustion_dead_letters_without_a_fresh_attempt`) against
hand-inserted rows and a single mid-sequence restart. This fixture adds
the pipeline-level pin: a runner-written row, the manifest-scripted
first failure, a restart before EVERY attempt, and the failure card
observed on the adapter.
