---
name: cli-channel
description: Use the CLI (stdin/stdout) channel for local development and testing — wiring, IO format, and limitations.
---

# cli-channel

The CLI channel is the simplest adapter in the registry. It has two
modes:

1. **stdio mode** — reads lines from stdin, writes labelled lines to
   stdout. The developer REPL for a foreground host, and the mode
   that makes end-to-end tests trivial (feed stdin from a fixture,
   capture stdout, assert).
2. **FIFO/log mode** — reads lines from a named pipe and appends one
   structured `CliFrame` JSON line per delivery to a log file. This
   is the bridge behind `cclaw chat`, and it survives writers
   opening and closing the FIFO.

Every input line becomes an `InboundEvent` in both modes.

## Wiring

The CLI factory is always registered in the in-tree host
`build_registry()`. To activate the channel:

1. Add a `ChannelInit` for it in the host's configuration:

```toml
[[channels]]
channel_type = "cli"
config = { label = "agent> " }
```

2. Create a messaging group with `channel_type = "cli"` and
   `platform_id = "stdin"` via `cclaw messaging-groups create`.
3. Create a wiring from that messaging group to an agent group via
   `cclaw wirings create --mg <mg> --ag <ag> --engage pattern --pattern '.*'`.

After boot, every line you type on stdin reaches the wired agent.

## Inbound format

A single line from stdin becomes one `InboundEvent`:

```json
{
  "channel_type": "cli",
  "platform_id": "stdin",
  "thread_id": null,
  "message": {
    "id": "<random uuid>",
    "kind": "chat",
    "content": { "text": "<the line>" },
    "timestamp": "<utc>",
    "is_mention": null,
    "is_group": null
  },
  "sender": {
    "channel_type": "cli",
    "identity": "local",
    "display_name": "local"
  }
}
```

The sender identity is always `"local"`. The CLI channel does not
distinguish between users; if you need multiple senders, write a
different harness.

## Outbound format

In stdio mode every outbound message is rendered as:

```text
<label><body>\n
```

The default label is `"agent> "`. Configure with `{"label": "..."}`.

Body rendering rules:

- If the outbound message's `content` is `{"text": "..."}`, that
  string is the body.
- Otherwise, the content is compact-JSON-serialised verbatim.
- If the message carries attachments, a `[files: a.txt, b.png]`
  suffix is appended.

In FIFO/log mode each delivery is instead one compact JSON `CliFrame`
line (e.g. `{"kind":"chat","text":"hello"}`), which is how structured
payloads (cards, todo lists, diffs) survive to the `cclaw chat`
renderer.

There is no native edit or reaction API: `edit_message` falls back to
a fresh `(edit) <text>` line, `add_reaction` to a
`(reaction: <emoji>)` line. No typing indicators are visible.

## Limitations

- One process, one sender. No DM concept (`open_dm` returns `None`).
- No threading (`supports_threads` is `false`).
- No platform message ids returned (`deliver` returns `Ok(None)`).
- No back-pressure on stdout. If your terminal is slow, the host's
  delivery loop blocks on `write_all`.
- In stdio mode the reader task ends on EOF: once stdin closes, the
  channel falls silent — restart the host to recover. FIFO/log mode
  holds its own writer handle open, so `cclaw chat` sessions can come
  and go freely.

## Programmatic use

For tests, do not use the factory; construct `CliAdapter` directly
with `CliAdapter::new_with_io(reader, writer, inbound_tx, label)`.
This avoids tying tests to the process's real stdin/stdout.

## Example session

```text
$ copperclaw run --config local.toml
copperclaw boot complete; idling
hello there
agent> Hi! How can I help?
schedule a daily standup at 9am
agent> Done. Task task_4b queued for 09:00 UTC each day.
```

## When to outgrow it

Any work that involves users other than yourself, persistent
context across restarts, or platform-native UI (buttons, cards,
threads) needs a real channel. The CLI channel is for local
iteration only.
