---
name: error-handling
description: Distinguish retryable from non-retryable errors in tool calls and delivery, and understand how the host retries on the agent's behalf.
---

# error-handling

Errors in ironclaw are layered. A tool call can fail at validation
time. A delivery can fail at the channel adapter. Either kind of
failure is either *retryable* (transient — try again later) or
*non-retryable* (the request is broken; stop). This skill explains
what to do for each.

## Tool-level errors

`ToolError` (in `ironclaw-mcp::error`) has three variants:

- `Validation(String)` — the arguments you sent are wrong. **Not
  retryable.** Examples: empty `text`, non-positive `message_id`,
  malformed base64 in `data`, `to.id` blank, missing required
  field. Fix the call and try again with different arguments.
- `Context(String)` — the host could not service the call (e.g.
  DB write failed, scheduler unreachable). **Sometimes retryable.**
  Retrying the same call after a brief pause is reasonable.
- `Internal(String)` — something panicked inside the tool. **Not
  retryable.** Surface to the user; do not loop.

Validation errors arrive on the agent's tool-result content with
`is_error = true` and a textual message. Read the message, adjust
the call, do not re-issue verbatim.

## Delivery-level errors

After a tool succeeds at the MCP layer, the row sits in
`outbound.db` until the host's delivery loop picks it up. The
delivery loop is your safety net — it retries on your behalf.

`AdapterError` (in `ironclaw-channels-core::error`):

- `Io(std::io::Error)` — transport-layer I/O failed.
  **Retryable.**
- `Transport(String)` — non-IO transport failure (HTTP 5xx,
  websocket close). **Retryable.**
- `Auth(String)` — credentials missing, expired, or rejected.
  **Not retryable** until an admin fixes the channel; the delivery
  loop will still try up to `MAX_DELIVERY_ATTEMPTS` (3).
- `Rate { retry_after }` — rate-limited. **Retryable**, honouring
  `retry_after` if present.
- `BadRequest(String)` — the adapter rejected the call. **Not
  retryable.** The row is marked failed after attempt 1.
- `Unsupported(String)` — the platform does not support this
  operation. **Not retryable.**
- `NotImplemented` — the adapter has not implemented this method.
  **Not retryable.**

## How the host retries delivery

The delivery loop runs two loops:

- **Active loop**, every 1 s, for sessions whose container is
  currently `Running`. Catches the common case where a reply must
  reach the user fast.
- **Sweep loop**, every 60 s, for every active session including
  idle ones.

On failure the row's `tries` increments and `deliver_after` is set
to `now + min(BACKOFF_BASE_MS * 2^(tries-1), ABSOLUTE_CEILING_MS)`
where `BACKOFF_BASE_MS = 5_000` and `ABSOLUTE_CEILING_MS = 1_800_000`
(30 minutes). After `MAX_DELIVERY_ATTEMPTS = 3` attempts the row is
marked `failed` and emitted to the `dropped_messages` table.

If `Rate { retry_after }` is returned with a value, the host
honours that instead of the exponential schedule for the next
attempt.

## What the agent should do

For tool-result `is_error = true`:

- If `Validation`: stop, inspect, change the arguments.
- If `Context` or `Internal`: surface to the user via
  `send_message` with a polite "I hit an internal error" message;
  do not silently loop.

For delivery failures the agent rarely sees directly. The host
records `delivered.status` rows for each outbound; an admin can
inspect via `iclaw dropped-messages list`. If a critical send did
not go through (e.g. an `ask_user_question` whose answer is
gating your workflow), schedule a follow-up with `schedule_task`
to re-check the situation.

## Idempotency

Outbound rows are uniquely identified by their seq. The delivery
loop sends each seq at most once successfully. If the channel
adapter cannot guarantee idempotent delivery (most cannot for
free-form text), assume the user *might* see duplicate messages
under aggressive retry conditions; design copy to be safe under
re-delivery (no "click here exactly once" links).

## Useful patterns

- Treat `Validation` errors as a learning event in the prompt
  context — the next call should be different.
- Treat `Rate` as guidance, not a wall; if the host retries for
  you, you do not need to back off in the prompt logic.
- For long-running operations, post a quick "starting" message
  before doing the work, so the user sees activity even if the
  final message gets retried.
