---
name: error-handling
description: Distinguish retryable from non-retryable errors in tool calls and delivery, and understand how the host retries on the agent's behalf.
---

# error-handling

Errors in copperclaw are layered. A tool call can fail at validation;
delivery can fail at the channel adapter. Either layer's failure is
either *retryable* (transient) or *non-retryable* (request is broken).

## Tool-level errors

`ToolError` (in `copperclaw-mcp::error`):

- `Validation(String)` — arguments are wrong. **Not retryable.**
  Examples: empty `text`, non-positive `message_id`, malformed base64,
  blank `to.id`, missing required field. Fix the call.
- `Context(String)` — host could not service it (DB write failed,
  scheduler unreachable). **Sometimes retryable** after a brief pause.
- `Internal(String)` — tool panicked. **Not retryable.** Surface to
  user; do not loop.

Validation errors arrive on the tool-result content with `is_error =
true` and a message. Read it, adjust, do not re-issue verbatim.

## Delivery-level errors

After the MCP layer succeeds, the row sits in `outbound.db` until the
host's delivery loop picks it up. The delivery loop is your safety net.

`AdapterError` (in `copperclaw-channels-core::error`):

- `Io(std::io::Error)` — transport I/O. **Retryable.**
- `Transport(String)` — non-IO transport (HTTP 5xx, ws close).
  **Retryable.**
- `Auth(String)` — credentials missing / expired / rejected. **Not
  retryable** until an admin fixes the channel; loop still tries up
  to `MAX_DELIVERY_ATTEMPTS` (3).
- `Rate { retry_after }` — rate-limited. **Retryable**, honouring
  `retry_after` when present.
- `BadRequest(String)` — adapter rejected. **Not retryable.** Marked
  failed after attempt 1.
- `Unsupported(String)` — platform doesn't support this. **Not
  retryable.**
- `NotImplemented` — adapter method not implemented. **Not
  retryable.**

## How the host retries delivery

Two loops:

- **Active**, every 1 s, for `Running` sessions. Fast path.
- **Sweep**, every 60 s, all active sessions including idle.

On failure: `tries` increments; `deliver_after = now + min(BACKOFF_BASE_MS
* 2^(tries-1), ABSOLUTE_CEILING_MS)` where `BACKOFF_BASE_MS = 5_000`
and `ABSOLUTE_CEILING_MS = 1_800_000` (30 min). After
`MAX_DELIVERY_ATTEMPTS = 3` the row is marked `failed` and emitted to
`dropped_messages`.

`Rate { retry_after }` overrides the exponential schedule for the next
attempt.

## What the agent should do

For tool-result `is_error = true`:

- `Validation`: stop, inspect, change the arguments.
- `Context` / `Internal`: surface to the user via `send_message` with
  a polite "I hit an internal error" message; do not silently loop.

Delivery failures are mostly invisible to the agent. The host records
`delivered.status` rows; admins inspect via `cclaw dropped-messages
list`. If a critical send is gating your workflow (e.g. an
`ask_user_question`), schedule a follow-up with `schedule_task`.

## Idempotency

Outbound rows are uniquely identified by seq; the delivery loop sends
each successfully at most once. Most adapters can't guarantee idempotent
free-form text delivery — under aggressive retry the user *might* see
duplicates. Design copy to be safe under re-delivery (no
"click here exactly once" links).

## Useful patterns

- Treat `Validation` as a learning event — the next call should
  differ.
- Treat `Rate` as guidance, not a wall; the host backs off for you.
- For long-running ops, post a quick "starting" message so the user
  sees activity even if the final message gets retried.
