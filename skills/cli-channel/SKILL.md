---
name: cli-channel
description: Use the CLI (stdin/stdout) channel for local development and testing — wiring, IO format, and limitations.
---

# cli-channel

The CLI channel is the simplest adapter in the registry. It reads
lines from stdin and writes lines to stdout. It exists for two
reasons:

1. To give developers an interactive REPL against a running host
   without configuring a real chat platform.
2. To make end-to-end tests trivial — feed stdin from a fixture
   file, capture stdout, assert.

Every input line becomes an `InboundEvent`; every outbound message
becomes a labelled line.

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

Every outbound message is rendered as:

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

There is no edit support: `edit_message` simply emits a new line.
Reactions emit a `reacted: <emoji>` line. No typing indicators are
visible.

## Limitations

- One process, one sender. No DM concept (`open_dm` returns `None`).
- No threading (`supports_threads` is `false`).
- No platform message ids returned (`deliver` returns `Ok(None)`).
- No back-pressure on stdout. If your terminal is slow, the host's
  delivery loop blocks on `write_all`.
- The reader task ends on EOF. Once stdin closes, the channel falls
  silent — restart the host to recover.

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
