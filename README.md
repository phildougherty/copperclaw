# Ironclaw

A lightweight, secure personal AI assistant runtime that runs Claude
agents in isolated Linux containers. Written in Rust; ships as a single
compiled binary plus a thin admin client.

Architecture in one paragraph: one Docker (or Apple Container) per
session, spawned on demand. Host-to-container IPC is
SQLite-on-bind-mount — each session has its own `inbound.db` (host
writes, container reads) and `outbound.db` (container writes, host
reads); a central `ironclaw.db` holds identity and wiring. Channels
(Telegram, Slack, Discord, plus 16 more) feed a router that resolves
sessions and writes inbound messages; the container's poll loop calls
Claude and writes outbound messages; the host's delivery loop
dispatches via the channel adapter.

See `PLAN.md` for the team-by-team design and milestone history.
`docs/` has the operator-facing references.

## Status

0.1.0 candidate. The workspace covers M0 through M10 of `PLAN.md` and
the M11 documentation set; the differential-replay harness lands as
the final M11 deliverable.

- **19 in-tree channel crates**: cli, telegram, slack, discord,
  resend, github, linear, webex, matrix, teams, gchat, whatsapp-cloud,
  signal, deltachat, emacs, x, wechat (Work Weixin), imessage, and a
  whatsapp (native Baileys) skeleton with a stubbed `CryptoBackend`.
- **Provider variants**: Anthropic HTTP-streaming with tool-use and
  compaction, plus subprocess-bridged Codex / OpenCode and an
  Ollama-via-Anthropic-base-URL variant.
- **Full host pipeline**: router, delivery (1s active + 60s sweep,
  exponential backoff, 3-attempt cap), and a 60s sweep loop for stuck
  detection / recurrence fanout / processing-ack reset.
- **OneCLI gateway** for centralised credential issuance with full
  wiremock coverage of 401/404/409/429/5xx and `Retry-After`.
- **`iclaw` admin client** over a Unix socket inside the host (41
  documented commands; CLI-scope-aware so agents can call read paths
  but not mutations).
- **Interactive setup binary** (`ironclaw setup`) with systemd /
  launchd unit generators and a `--migrate-from` data-directory
  migrator.
- **17 authored skills** under `skills/`.
- **4406 passing tests**, 0 failing. `cargo clippy --workspace
  --all-targets -- -D warnings` clean. CI runs fmt + clippy + test on
  Linux and macOS with an 85% coverage gate.

## Build

Requires Rust 1.85+ (pinned via `rust-toolchain.toml`).

```
cargo build --workspace
cargo test --workspace
```

The release artifacts are:

- `target/release/ironclaw` — host orchestrator binary.
- `target/release/iclaw` — admin client.
- `target/release/ironclaw-setup` — interactive setup helper.

## Quick start

```
# First-time setup walks every step with defaults you can override.
ironclaw setup

# Boot the host. Defaults to data-dir = ~/.local/share/ironclaw.
ironclaw run

# In another terminal, drive it via iclaw.
iclaw groups list
iclaw sessions list --status active
```

For headless / scripted installs, set the `IRONCLAW_SETUP_*` env vars
and pass `--non-interactive` to `ironclaw setup`. See
`docs/cutover.md` for migrating from a predecessor data directory.

## Documentation

- [`PLAN.md`](PLAN.md) — full team-by-team design and milestone
  progress.
- [`docs/adding-a-channel.md`](docs/adding-a-channel.md) — how to ship
  a new channel adapter.
- [`docs/cutover.md`](docs/cutover.md) — operator playbook for
  switching a predecessor installation onto Ironclaw.
- [`docs/replay-fixtures.md`](docs/replay-fixtures.md) — design of
  the differential-replay test harness.
- [`docs/release-checklist.md`](docs/release-checklist.md) — release
  procedure.

## License

MIT — see [`LICENSE`](LICENSE).
