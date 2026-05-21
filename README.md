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

0.1.0 candidate. The workspace covers M0 through M10 of `PLAN.md`,
the M11 documentation set, the M12 chat-loop slice, the M13
operational-hardening slice, M14 (agent capability — tool wiring +
computer-use), and the M14 follow-up that auto-loads skill content
into the system prompt.

- **21 in-tree channel crates**: cli, telegram, slack, discord,
  resend, github, linear, webex, matrix, teams, mattermost (REST v4
  + outgoing-webhook ingress), line (Messaging API; HMAC-SHA256
  signed inbound + free-reply / paid-push egress), gchat,
  whatsapp-cloud (Meta Cloud Business API), webhooks (generic
  HMAC-signed HTTP inbound — one adapter for Stripe / Grafana /
  Sentry / Vercel / Shopify / IoT / hand-rolled CI hooks), signal
  (via signal-cli RPC), deltachat, emacs, x, wechat (Work Weixin),
  imessage. Every shipped channel is a complete implementation —
  no stubbed crypto, no placeholder backends.
- **Provider variants**: Anthropic HTTP-streaming with tool-use and
  compaction, plus subprocess-bridged Codex / OpenCode and an
  Ollama-via-Anthropic-base-URL variant. `ironclaw-setup` accepts
  `openrouter` as a friendly shortcut at the base-URL prompt.
- **Agent tool inventory**: 20 in-tree tools — 15 messaging /
  scheduling / self-mod (`send_message`, `send_file`,
  `edit_message`, `add_reaction`, `ask_user_question`, `send_card`,
  `create_agent`, `install_packages`, `add_mcp_server`,
  `schedule_task`, `list_tasks`, `cancel_task`, `pause_task`,
  `resume_task`, `update_task`), 4 computer-use (`shell`,
  `read_file`, `write_file`, `web_fetch`), and 1 multi-provider
  web search (`web_search` — routes to Tavily / Exa / Brave /
  SerpAPI based on which API key the operator has wired up).
- **System-prompt skill auto-load**: when `IRONCLAW_SKILLS_DIR` is
  set, the container manager inlines each enabled SKILL.md body
  into the runner's system prompt at spawn so the model knows
  *when* to reach for each tool.
- **Full host pipeline**: router, delivery (1s active + 60s sweep,
  exponential backoff, 3-attempt cap), a 60s sweep loop for stuck
  detection / recurrence fanout / processing-ack reset, and a
  container manager that reconciles per-session state across
  Stopped / Idle / Running with heartbeat-driven crash-restart,
  configurable idle-stop (5-minute default), per-group
  `egress_allow` + `resource_limits`, and automatic image rebuild
  on `container_configs` diff (sha256 fingerprint over
  `packages_apt` / `packages_npm` / `skills` / `mcp_servers`;
  rebuild failures fall back to the last-known-good tag and emit
  `ironclaw_image_rebuild_failed_total`).
- **Operator surface**: `iclaw health` (one-shot probe — session
  breakdown by container_status, recent mutations, dropped-message
  count), `iclaw audit list` (append-only mutation log;
  truncated-args, latency_ms, caller kind, env-value redaction on
  sensitive commands), `iclaw usage` (per-group token rollup from
  `agent_turns` the runner populates from provider `usage`
  events), `iclaw budgets set` (per-group `daily_token_cap`;
  manager refuses to spawn when today's tokens exceed the cap),
  `iclaw approvals approve` (persistent sender approval; gate
  consults the central `users` table on every inbound, with
  in-channel "approve?" notifications on first contact),
  `iclaw chat` (interactive REPL against the install's cli
  channel), `iclaw db backup <path>` (WAL-checkpointed atomic
  copy), `iclaw dropped-messages outbound-list` /
  `iclaw dropped-messages replay <id>` (dead-letter
  inspect/replay), `iclaw mcp list-presets` /
  `iclaw mcp add <preset>` (curated MCP server library:
  postgres, linear, github, notion, filesystem, browserbase), and
  `iclaw groups config set-egress-allow` /
  `set-resource-limits`.
- **Observability** (opt-in): Prometheus `/metrics` endpoint via
  `IRONCLAW_METRICS_ADDR=127.0.0.1:9090` (bare port shorthand
  supported); counters for inbound / outbound / spawned /
  crashed / delivery-failed / image-rebuild-failed; histograms
  for LLM call seconds, input / output tokens, container spawn
  seconds. Log rotation via `IRONCLAW_LOG_DIR=<path>` enables a
  daily-rotating file appender alongside stderr. Off by default.
  See [`docs/observability.md`](docs/observability.md).
- **OneCLI gateway** for centralised credential issuance with full
  wiremock coverage of 401/404/409/429/5xx and `Retry-After`.
- **`iclaw` admin client** over a Unix socket inside the host (50+
  wire commands plus composite client-side fan-outs;
  CLI-scope-aware so agents can call read paths but not mutations;
  every mutation lands in `audit_log` with env-value redaction on
  sensitive commands).
- **Interactive setup binary** (`ironclaw-setup`) with systemd /
  launchd unit generators (including `Restart=on-failure`) and a
  `--migrate-from` data-directory migrator. Writes
  `IRONCLAW_SKILLS_DIR` + `IRONCLAW_GROUPS_DIR` so the install's
  skills directory is discoverable on first boot.
- **22 authored skills** under `skills/`.
- **4576 passing tests**, 0 failing. `cargo clippy --workspace
  --all-targets -- -D warnings` clean. CI runs fmt + clippy + test on
  Linux and macOS with an 85% coverage gate.
- **End-to-end chat works** against any Anthropic-API-compatible
  provider (Anthropic native or OpenRouter via
  `ANTHROPIC_BASE_URL=https://openrouter.ai/api/v1`). The host's
  container manager spawns a runner-in-container per session, the
  runner reads inbound.db, calls the provider, writes outbound.db
  plus a `usage_report` system row (which the delivery loop folds
  into `agent_turns`), marks the inbound completed, and the
  delivery loop fans the reply back via the channel adapter.
  Verified live against OpenRouter: typed
  `What is 7*8? Number only.` through `iclaw chat`, got back
  `agent> 56`, `iclaw usage` recorded 7787 input + 7 output
  tokens.

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
# Picks an install root per-platform (Linux: $XDG_DATA_HOME/ironclaw,
# falling back to ~/.local/share/ironclaw; macOS: ~/Library/Application
# Support/ironclaw). Pass --data-dir to override. At the provider-
# URL prompt, type `openrouter` (or leave blank for Anthropic).
ironclaw-setup

# Boot the host. With no `--env-file`, ironclaw auto-discovers the
# .env setup wrote inside the platform install root, so both the data
# dir and the iclaw socket path get picked up automatically.
ironclaw run

# In another terminal — iclaw also resolves the install's socket
# without configuration, so the commands Just Work from any cwd.
iclaw quickstart cli --name first    # group + mg + wiring in one call
iclaw status                          # full wiring digest
iclaw health                          # operator probe (sessions, audit, drops)
iclaw chat                            # interactive REPL against the cli channel
iclaw usage --since 24h               # per-group token rollup
iclaw audit list --since 1h           # mutations against the host socket
```

`ironclaw run` resolution order for the `.env`: `--env-file <path>` →
`./.env` → platform install (`$XDG_DATA_HOME/ironclaw/.env` on Linux,
`~/Library/Application Support/ironclaw/.env` on macOS). With none of
those, the host falls back to `IRONCLAW_DATA_DIR=./data` so
`cargo run -p ironclaw-host` from a checkout still works.

`iclaw` resolves the socket in the same order: `--socket` → `ICLAW_SOCKET`
→ platform install → `./data/iclaw.sock`. Run
`iclaw completions <bash|zsh|fish>` to drop a completion script into
your shell.

For headless / scripted installs, pass `--headless` (alias
`--non-interactive`) to `ironclaw-setup` and supply each prompt as an
`IRONCLAW_SETUP_*` env var. The only required variable is the API
key; the rest have sensible defaults so an unattended install can be
as short as:

```
IRONCLAW_SETUP_ANTHROPIC_API_KEY=sk-ant-... ironclaw-setup --headless
```

The full set of overrides for a CLI-only install:

```
IRONCLAW_SETUP_ANTHROPIC_API_KEY   # required
IRONCLAW_SETUP_USE_ONECLI=no       # default: no
IRONCLAW_SETUP_BUILD_IMAGE=yes     # default: yes (`no` skips docker build)
IRONCLAW_SETUP_MOUNTS=             # comma-separated host paths, default empty
IRONCLAW_SETUP_WRITE_SERVICE_UNIT=no  # default: no
IRONCLAW_SETUP_TIMEZONE=Etc/UTC    # default: detect from system
IRONCLAW_SETUP_FIRST_CHANNEL=cli   # default: cli
```

Run `ironclaw-setup --list-steps` for the canonical list and use
`--skip-step <name>` for any optional step you want to leave for later.
See `docs/cutover.md` for migrating from a predecessor data directory.

## Documentation

- [`PLAN.md`](PLAN.md) — full team-by-team design and milestone
  progress.
- [`docs/adding-a-channel.md`](docs/adding-a-channel.md) — how to ship
  a new channel adapter.
- [`docs/container-config.md`](docs/container-config.md) — per-group
  image rebuild, egress allow-list, and resource caps.
- [`docs/observability.md`](docs/observability.md) — Prometheus
  metrics endpoint and log rotation.
- [`docs/db-backup.md`](docs/db-backup.md) — central DB backup and
  restore procedure.
- [`docs/web-search.md`](docs/web-search.md) — multi-provider
  `web_search` tool: Tavily / Exa / Brave / SerpAPI.
- [`docs/cutover.md`](docs/cutover.md) — operator playbook for
  switching a predecessor installation onto Ironclaw.
- [`docs/replay-fixtures.md`](docs/replay-fixtures.md) — design of
  the differential-replay test harness.
- [`docs/release-checklist.md`](docs/release-checklist.md) — release
  procedure.

## License

MIT — see [`LICENSE`](LICENSE).
