---
name: typing-indicator
description: How typing indicators are emitted by the host typing module — when they fire, what they signal, and the throttling rules.
---

# typing-indicator

The host runs a `TypingModule` that calls each channel adapter's
`set_typing` whenever the agent is "doing work" on a thread. This is
visible to humans as the "Bot is typing…" hint and is the only outward
signal of progress between the moment a message arrives and the
moment a reply is delivered.

## When typing indicators are emitted

Two preconditions:

1. The container has work in flight — i.e. `container_state.current_tool`
   is set. The runner writes this row whenever it enters a tool call
   (e.g. an MCP tool, a provider streaming step) and clears it on exit.
2. The session has at least one routable target — i.e.
   `session_routing` has `(channel_type, platform_id, …)` set.

The host's sweep loop ticks at a configurable cadence (4 s by default
in `TypingConfig::interval_ms`) and, for every active container, calls
`adapter.set_typing(platform_id, thread_id)` on the routing target.

## What the indicator signals

To the user: "your message was received and we're processing it."

To you, the agent: nothing. The typing indicator is emitted **for** you
by the host — you do not need to do anything to keep it ticking. If
you want to stop the indicator early (e.g. because a tool call will
take minutes and you'd rather not look stuck), explicitly send a
status `send_message("Working on the report; this will take a few
minutes.")`.

## Throttling

Indicators are throttled per `(channel_type, platform_id, thread_id)`:
- Slack, Telegram, and Discord render a typing hint for about 5-7
  seconds after the last call. 4 s spacing gives a comfortable
  refresh cadence without spamming the API.
- If the agent's tool call returns inside the first interval, no
  duplicate indicator is sent.
- If the same thread is in flight for two consecutive ticks, the
  module fires once per tick (throttle expires).

The throttle is in-memory on the host and resets on host restart.

## What each adapter does

- **Slack**: calls `chat.startTyping` (the post-2024 SCIM-aware
  variant) on the destination channel.
- **Telegram**: calls `sendChatAction(action="typing")`.
- **Discord**: calls `POST /channels/{id}/typing`.
- **CLI**: silent no-op; the indicator would not be visible on stdio.
- Channels that do not support typing return `Ok(())` from the
  default `set_typing` impl. There is no error.

## Config

`TypingConfig` (in `ironclaw-modules::typing`) is:

```rust
TypingConfig {
    enabled: true,
    interval_ms: 4000,
}
```

Hosts can override this via configuration; admins can disable typing
entirely with `enabled = false`.

## Why not have agents emit typing themselves?

Earlier designs let the agent call `set_typing`. We removed that
because:

- The agent does not know the wall-clock cadence the platform needs.
- Per-platform throttling rules are tedious to embed in prompts.
- The host already knows when the container is busy
  (`container_state.current_tool` is set), so it can emit indicators
  without an agent decision.

The right pattern is: do your work normally; the host signals
typing on your behalf.
