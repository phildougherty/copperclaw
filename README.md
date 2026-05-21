# Ironclaw

A secure, self-hosted runtime for Claude-style AI agents. Spawns one
isolated Linux container per session, brokers traffic between 21
real-world messaging channels, and exposes a single-binary admin
surface for operations, budgets, and audit.

Written in Rust. One host binary, one admin client, one setup wizard
— no daemons spawning daemons, no hidden state, no half-finished
adapters in the tree.

```
> What's the capital of France? One word only.
agent> Paris

> Reply with just a haiku about containers.
agent> Boxes hold the world,
       Isolated, yet deployed—
       Code sails everywhere.
```

(Verified live against OpenRouter through the CLI channel.)

---

## Highlights

- **21 channel adapters** in-tree, every one a complete
  implementation: Telegram, Slack, Discord, Matrix, Microsoft Teams,
  Google Chat, Mattermost, LINE, Webex, WhatsApp Cloud, Signal,
  Delta Chat, iMessage, WeChat Work, Emacs, X/Twitter, Linear,
  GitHub, Resend, generic HMAC-signed webhooks, plus a local CLI
  channel for development and scripting.
- **One container per session.** Sessions are durable; containers
  are ephemeral and restartable. State lives in SQLite files on a
  bind-mount — `inbound.db` (host writes, container reads) and
  `outbound.db` (container writes, host reads) — plus a central
  identity / wiring database. The single-writer rule is enforced
  by code, not convention.
- **First-class agent tools.** 20 in-tree tools the model can call:
  send / edit / react / file / card / question, schedule and
  manage recurring tasks, install packages, register MCP servers,
  spawn sibling agents, plus four computer-use tools (`shell`,
  `read_file`, `write_file`, `web_fetch`) and a multi-provider
  `web_search` tool that auto-routes to Tavily / Exa / Brave /
  SerpAPI based on which API key is configured.
- **Multiple providers.** Anthropic native (HTTP streaming with
  tool use and automatic compaction), Anthropic-compatible
  gateways (OpenRouter, internal proxies — set
  `ANTHROPIC_BASE_URL`), subprocess-bridged Codex / OpenCode, and
  Ollama via the Anthropic shim.
- **Operations built in.** Per-group token budgets and turn-rate
  caps, persistent sender approvals with in-channel prompts,
  dead-letter inspection and replay, audit logs of every host
  mutation (with env-value redaction on sensitive commands),
  Prometheus metrics endpoint, log rotation, SIGHUP secret
  rotation, central-DB backup / restore.
- **Reproducible images.** Per-agent-group image fingerprints
  (sha256 over `packages_apt` / `packages_npm` / `skills` /
  `mcp_servers`) trigger automatic rebuilds on config diff.
  Rebuild failures fall back to the last-known-good tag and emit a
  metric so the operator notices.
- **Conservative defaults.** Idle-stop timeout in minutes, not
  hours. Retry cap of three. Webhook channels bind `127.0.0.1` by
  default. Opt-in budgets, opt-in metrics endpoint, opt-in log
  rotation. Surprises cost money; surprises don't ship here.
- **Tested.** ~4634 passing tests, 0 failing.
  `cargo clippy --workspace --all-targets -- -D warnings` clean.
  CI runs fmt + clippy + test on Linux and macOS with an 85%
  coverage gate. Differential replay-fixture harness covers the
  end-to-end inbound-route → runner → outbound-deliver pipeline.

---

## Install

Ironclaw needs Rust 1.85+ (pinned by `rust-toolchain.toml`) and a
container runtime (Docker on Linux, Docker or Apple Container on
macOS).

```bash
git clone https://github.com/example/ironclaw
cd ironclaw
cargo build --release --workspace
```

Three binaries land in `target/release/`:

| Binary | Role |
| --- | --- |
| `ironclaw` | The host orchestrator. Long-running; serves the inbound router, the outbound delivery loop, the per-session container manager, and the local admin socket. |
| `iclaw` | Admin client. Talks to the host's Unix socket. Read paths are open to in-container agents; mutations are host-only. |
| `ironclaw-setup` | Interactive one-time installer. Writes `.env`, builds the container image, drops a systemd unit or launchd plist, and creates a default CLI agent group so the first chat works. |

A pre-built Debian-slim container image is produced as part of
setup; rebuilds are automatic on config change.

---

## Quickstart

The fast path from zero to a working chat:

```bash
ironclaw-setup              # interactive; press Enter to accept defaults
ironclaw run                # boot the host; idles waiting for inbound
```

In a second terminal:

```bash
iclaw doctor                # diagnose; every non-OK row prints a `fix:`
iclaw chat                  # interactive REPL against the CLI channel
```

`ironclaw-setup` auto-creates a default `cli/stdin` messaging group
wired to an agent group named `first` with session mode `shared`,
so `iclaw chat` Just Works on the very first `ironclaw run`. Opt
out with `IRONCLAW_SETUP_QUICKSTART=no`.

### Headless / scripted install

```bash
IRONCLAW_SETUP_ANTHROPIC_API_KEY=sk-ant-... ironclaw-setup --headless
```

The only required variable is the provider API key. Override any
prompt by setting the matching env var; run `ironclaw-setup
--list-steps` for the canonical list, or `--skip-step <name>` to
defer a step. See the per-prompt table below.

| Variable | Default | Purpose |
| --- | --- | --- |
| `IRONCLAW_SETUP_ANTHROPIC_API_KEY` | _required_ | Provider API key (Anthropic or compatible). |
| `IRONCLAW_SETUP_USE_ONECLI` | `no` | Enable OneCLI credential gateway. |
| `IRONCLAW_SETUP_BUILD_IMAGE` | `yes` | Build the session container image during setup. |
| `IRONCLAW_SETUP_MOUNTS` | `` (empty) | Comma-separated host paths to bind-mount read-only into every session. |
| `IRONCLAW_SETUP_WRITE_SERVICE_UNIT` | `no` | Drop a systemd/launchd unit. |
| `IRONCLAW_SETUP_TIMEZONE` | system | Container timezone. |
| `IRONCLAW_SETUP_FIRST_CHANNEL` | `cli` | Which channel to wire first. |
| `IRONCLAW_SETUP_QUICKSTART` | `yes` | Auto-create the default CLI agent group + wiring. |

### Choosing a provider

At the provider-URL prompt, type `openrouter` (or `or`) to use
OpenRouter, leave blank or type `anthropic` for the upstream API,
or paste any Anthropic-compatible base URL verbatim — a trailing
`/v1` is stripped automatically. The provider key is then
forwarded into every session container via
`ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL`.

---

## Channels

| Channel | Ingress | Egress | Notes |
| --- | --- | --- | --- |
| `cli` | stdin | stdout | Local REPL for development. |
| `telegram` | webhook or long-poll | Bot API | Inbound attachment download (text, photo, audio, video, voice, doc) with size cap. |
| `slack` | events API | Web API | HMAC-SHA256 signature verification, files v2 upload. |
| `discord` | slim gateway | REST | Pure codec/lifecycle parsers; gateway intent `38_401`. |
| `matrix` | `/sync` long-poll | Client-Server REST | Threads via `m.relates_to`; alias resolution cached. |
| `teams` | change-notifications webhook | Graph REST | Validation handshake + constant-time `clientState` compare. |
| `gchat` | HTTP push | REST v1 | `cardsV2` for cards; emoji shortcode map. |
| `mattermost` | outgoing webhook | REST v4 | Free-reply / paid-push split. |
| `line` | webhook (HMAC-SHA256) | Messaging API | Free reply via reply-token; paid push fallback. |
| `webex` | webhook (HMAC-SHA1/256) | REST | Body fetch via `GET /messages/{id}` because webhooks omit text. |
| `whatsapp-cloud` | webhook (Meta Cloud) | Graph | `hub.verify_token` handshake + `X-Hub-Signature-256`. |
| `signal` | signal-cli RPC | signal-cli RPC | `RpcTransport` trait — tests never spawn `signal-cli`. |
| `deltachat` | `deltachat-rpc-server` | RPC | Inbound attachments via `download_full_msg` + stat/open. |
| `imessage` | sqlite tail + osascript | osascript | macOS-only; Cocoa-epoch detection. |
| `wechat` | webhook (Work Weixin) | REST | Hand-rolled AES-256-CBC + SHA1 over sorted concat. |
| `emacs` | emacsclient | emacsclient | `EmacsClient` trait — tests never spawn emacs. |
| `x` | poll (`/2/dm_events`) | v2 DMs + v1.1 media | Since-id persisted to disk. |
| `linear` | webhook (HMAC-SHA256) | GraphQL | `commentCreate` / `commentUpdate` / `reactionCreate`. |
| `github` | webhook (HMAC-SHA256) | REST | LRU dedup on `X-GitHub-Delivery`. |
| `resend` | _none_ | REST | Send-only email; no reply surface. |
| `webhooks` | generic HMAC | _none_ | One adapter for Stripe / Grafana / Sentry / Vercel / Shopify / IoT. |

Each channel ships behind the same `ChannelAdapter` trait; see
[`docs/adding-a-channel.md`](docs/adding-a-channel.md) for the
template.

---

## Agent tools

The runner inside each container exposes 20 tools to the model:

**Messaging.** `send_message`, `send_file`, `edit_message`,
`add_reaction`, `ask_user_question`, `send_card`.

**Scheduling.** `schedule_task`, `list_tasks`, `cancel_task`,
`pause_task`, `resume_task`, `update_task`.

**Self-modification.** `install_packages` (apt / npm — image
rebuilds on next spawn), `add_mcp_server` (MCP transport
registration), `create_agent` (spin up a sibling agent group).

**Computer use.** `shell` (bash inside the container, 60s default
timeout, 64 KiB output cap), `read_file` (UTF-8, 1 MiB cap),
`write_file` (auto-mkdir-p, create or append), `web_fetch` (HTTP
GET/POST, 256 KiB body cap, 30s default).

**Web search.** `web_search` with a normalised
`{title, url, snippet, published?, score?}` shape, routing
automatically based on which key is configured: `TAVILY_API_KEY`,
`EXA_API_KEY`, `BRAVE_SEARCH_API_KEY`, or `SERPAPI_API_KEY`. Per-
call result cap 1–25 (default 10), UTF-8-safe snippet truncation
at 4 KiB.

Per-skill SKILL.md prose is auto-inlined into the runner's system
prompt at spawn so the model knows *when* to reach for each tool,
not just what each tool's schema looks like. 22 skill bundles are
authored under `skills/`.

---

## Operator commands

`iclaw` is the local admin client; it talks to the host's Unix
socket. The most-used commands:

```bash
iclaw doctor                          # composite first-run / ongoing diagnostic
iclaw health                          # one-shot probe — session breakdown + audit + drops
iclaw chat                            # interactive REPL against the cli channel
iclaw status                          # wiring digest

iclaw groups list                     # configured agent groups
iclaw messaging-groups list           # channel groups (e.g. slack/C12345)
iclaw wirings list                    # which messaging group → which agent group

iclaw approvals list                  # pending sender approvals
iclaw approvals approve --channel telegram --identity 12345

iclaw usage --since 24h                # per-group token rollup
iclaw budgets set --agent-group-id <id> --daily-tokens 100000
iclaw budgets set --agent-group-id <id> --turns-per-minute 4

iclaw audit list --since 1h            # mutation log
iclaw schema-version                   # central-DB schema check

iclaw mcp list-presets                 # curated MCP servers
iclaw mcp add postgres --agent-group-id <id> --env DATABASE_URL=...
iclaw groups config set-resource-limits <id> --cpus 1 --memory-mb 1024
iclaw groups config set-egress-allow <id> example.com:443

iclaw dropped-messages outbound-list --since 24h
iclaw dropped-messages replay <id>

iclaw db backup /backups/ironclaw.sqlite
iclaw db restore /backups/ironclaw.sqlite  # only when host is stopped

iclaw completions <bash|zsh|fish>      # drop a completion script
```

Run any command with `--json` for machine-readable output. Read
paths are callable by in-container agents; mutations are
host-only.

---

## Architecture

```
                ┌──────────────────────┐
                │ External channel     │  (Telegram, Slack, ...)
                └──────────┬───────────┘
                           │  webhook / gateway
                  ┌────────▼────────┐
                  │  Channel adapter│
                  └────────┬────────┘
                           │  InboundEvent
                  ┌────────▼────────┐
                  │     Router      │  resolve session, fan out
                  └────────┬────────┘
                           │  writes
                  ┌────────▼────────┐
                  │   inbound.db    │  per-session SQLite, journal=DELETE
                  └────────┬────────┘
                           │  bind-mount (RO from container)
        ╔══════════════════▼══════════════════╗
        ║ Session container                   ║
        ║   poll loop → provider (Anthropic)  ║
        ║         │     tool-use loop          ║
        ║         └→ rmcp client / handlers   ║
        ║                │                    ║
        ║         ┌──────▼──────┐              ║
        ║         │ outbound.db │              ║
        ║         └──────┬──────┘              ║
        ╚════════════════│════════════════════╝
                         │  host-poll
                  ┌──────▼──────┐
                  │  Delivery   │  active 1s, sweep 60s
                  └──────┬──────┘
                         │  ChannelAdapter::deliver
                  ┌──────▼──────┐
                  │ External    │
                  │ recipient   │
                  └─────────────┘

Background loops on host:
  - Active delivery poll  (1s, running sessions)
  - Sweep delivery poll   (60s, all active sessions)
  - Sweep                 (60s, stuck detection, recurrence, heartbeat)
  - Container manager     (1s, reconcile Stopped/Idle/Running)
  - iclaw socket server   (Unix socket; newline-delimited JSON)
```

**Three invariants:**

1. `inbound.db` uses `journal_mode=DELETE`. WAL's shared-memory
   region doesn't propagate across the Docker bind-mount; silent
   data loss otherwise.
2. Each SQLite file has exactly one writer process.
3. Sessions are durable; containers are ephemeral. State lives in
   DBs and the filesystem, never in process memory beyond
   debounce / inflight maps.

---

## Configuration

`.env` keys the host reads at boot. `ironclaw-setup` writes a
populated copy; production overrides go in your service unit.

| Key | Purpose |
| --- | --- |
| `ANTHROPIC_API_KEY` | Provider key forwarded into every spawned container. |
| `ANTHROPIC_BASE_URL` | Optional Anthropic-compatible base URL (OpenRouter, internal proxy). Trailing `/v1` stripped automatically. |
| `IRONCLAW_DATA_DIR` | Host data root. Setup writes per-platform install path; defaults to `./data` when unset. |
| `ICLAW_SOCKET` | Override socket path; otherwise resolved per-platform. |
| `IRONCLAW_SKILLS_DIR` | Skills directory whose `SKILL.md` bodies get auto-inlined into the runner's system prompt at spawn. |
| `IRONCLAW_GROUPS_DIR` | Per-agent-group skill overrides under `<groups_dir>/<ag_uuid>/skills/`. |
| `IRONCLAW_METRICS_ADDR` | Bind address for the Prometheus endpoint (e.g. `127.0.0.1:9090`). Off when unset. |
| `IRONCLAW_LOG_DIR` | Enable daily-rotating file appender alongside stderr. Off when unset. |
| `IRONCLAW_DEFAULT_PROVIDER` | Provider name for sessions whose group hasn't pinned one. |
| `IRONCLAW_DEFAULT_IMAGE_TAG` | Default container image tag when no `container_configs` row pins one. |
| `TAVILY_API_KEY` / `EXA_API_KEY` / `BRAVE_SEARCH_API_KEY` / `SERPAPI_API_KEY` | Forwarded into the container so `web_search` auto-selects a backend. |

A SIGHUP on the host re-reads the `.env` file, updates the
forwarded keys, and increments the `ironclaw_secrets_rotated_total`
metric counter. Running containers see the rotated values after
the next idle-stop + respawn (default 5 minutes); for an immediate
rotation, `iclaw groups restart <id>`.

---

## Observability

Opt-in Prometheus endpoint:

```bash
IRONCLAW_METRICS_ADDR=127.0.0.1:9090 ironclaw run
```

Counters: `ironclaw_messages_inbound_total`,
`ironclaw_messages_outbound_total`,
`ironclaw_containers_spawned_total`,
`ironclaw_containers_crashed_total`,
`ironclaw_delivery_failed_total`,
`ironclaw_image_rebuild_failed_total`,
`ironclaw_secrets_rotated_total`.

Histograms: `ironclaw_llm_call_seconds`,
`ironclaw_llm_tokens_input`, `ironclaw_llm_tokens_output`,
`ironclaw_container_spawn_seconds`.

Log rotation (also opt-in):

```bash
IRONCLAW_LOG_DIR=/var/log/ironclaw ironclaw run
```

Writes one daily-rotated file alongside the stderr stream so
container output never contaminates the data path.

See [`docs/observability.md`](docs/observability.md) for the full
operator playbook.

---

## Documentation

Operator-facing guides:

- [`docs/adding-a-channel.md`](docs/adding-a-channel.md) —
  build a new channel adapter from the trait template.
- [`docs/container-config.md`](docs/container-config.md) —
  per-group image rebuild, egress allow-list, resource caps.
- [`docs/observability.md`](docs/observability.md) — metrics
  endpoint and log rotation.
- [`docs/db-backup.md`](docs/db-backup.md) — backup and restore
  the central SQLite database.
- [`docs/web-search.md`](docs/web-search.md) — the multi-provider
  `web_search` tool.
- [`docs/webhooks-tls.md`](docs/webhooks-tls.md) — TLS
  termination via Caddy / nginx / Cloudflare Tunnel.
- [`docs/cutover.md`](docs/cutover.md) — migrate from a
  predecessor installation onto Ironclaw.
- [`docs/replay-fixtures.md`](docs/replay-fixtures.md) — the
  differential-replay test harness and capture workflow.
- [`docs/release-checklist.md`](docs/release-checklist.md) —
  steps for cutting a release.

---

## Tenets

Ironclaw is built like an OpenBSD-style claw-agent runtime:

1. **No stubs in tree.** A half-implemented adapter is worse than
   no adapter — it lies to the registry and fails at message time.
2. **Secure-by-default, public-by-deliberate-act.** Every webhook
   binds `127.0.0.1` unless the operator explicitly chooses
   otherwise. The CLI channel pre-approves only the literal local
   sender. The `.env` is `0o600`.
3. **One process, one binary.** `ironclaw` is the host; `iclaw`
   is the admin client; `ironclaw-runner` runs inside containers.
   No daemons spawning daemons.
4. **Documentation is a deliverable.** Every crate's `lib.rs`
   doc-comment explains what the crate does, what its inputs are,
   and what the error paths mean.
5. **Conservative defaults.** Idle-stop in minutes; retries
   capped at 3; budgets opt-in; rate limits always present.
6. **Audit everything that mutates.** Every `iclaw` socket call
   that writes lands in `audit_log` with caller, command, args,
   result, and latency.
7. **Reproducible builds.** Image fingerprints include the
   runner binary bytes. Same source → same sha tag → same
   deployable artifact.
8. **Pinned upstreams.** Workspace deps version-pinned in
   `Cargo.toml`; `Cargo.lock` checked in; CI runs `cargo deny`.
9. **Errors over silent fallback.** A misconfigured channel
   fails loudly at boot. A bad webhook signature returns 401, not
   a quiet drop.

---

## Status

Pre-1.0. The end-to-end chat path works against any
Anthropic-API-compatible provider; the operator surface is
production-shaped; the replay-fixture harness pins the
inbound-route → runner → outbound-delivery pipeline against
byte-stable expected output. The remaining gap before tagging
0.1.0 is release-process polish — see
[`docs/release-checklist.md`](docs/release-checklist.md).

Contributions welcome. The codebase has a strong "no stubs in
tree" rule, so a new channel or tool should ship complete or not
at all; see
[`docs/adding-a-channel.md`](docs/adding-a-channel.md) for the
contract.

---

## License

MIT — see [`LICENSE`](LICENSE).
