# Ironclaw

A self-hosted runtime for Claude-style agents. Each session runs in
its own Linux container; the host wires 21 messaging-channel adapters
into a router on the inbound side and a delivery loop on the outbound
side. An admin client (`iclaw`) and a setup wizard (`ironclaw-setup`)
live alongside the host binary (`ironclaw`).

Written in Rust. Pre-1.0 — no tagged releases yet, no prebuilt binary
artifacts, install path is `cargo install --git` until the first tag
lands. Works end-to-end against any Anthropic-API-compatible provider.
Rough edges noted in [What's rough](#whats-rough).

```
> What's the capital of France? One word only.
agent> Paris

> Reply with just a haiku about containers.
agent> Boxes hold the world,
       Isolated, yet deployed—
       Code sails everywhere.
```

(Live against OpenRouter through the CLI channel.)

---

## What you get

- **21 channel adapters**: Telegram, Slack, Discord, Matrix, Microsoft
  Teams, Google Chat, Mattermost, LINE, Webex, WhatsApp Cloud, Signal,
  Delta Chat, iMessage, WeChat Work, Emacs, X/Twitter, Linear,
  GitHub, Resend, generic HMAC-signed webhooks, and a local `cli`
  channel for development. Coverage varies — see the per-channel docs
  under [`docs/channels/`](docs/channels/) for what each adapter
  actually implements vs. what's documented as Unsupported.
- **One container per session.** Sessions are durable; containers are
  ephemeral and restartable. State lives in SQLite files on a
  bind-mount (`inbound.db` written by the host, `outbound.db` written
  by the container) plus a central identity / wiring DB.
- **36 in-tree tools the model can call**, grouped: messaging
  (`send_message` / `send_file` / `edit_message` / `add_reaction` /
  `ask_user_question` / `send_card`), scheduling (`schedule_task` and
  five companions — backed by a real cron-evaluating sweep loop in
  the host), self-modification (`install_packages`, `add_mcp_server`,
  `create_agent`), computer use (`shell`, `read_file`, `write_file`,
  `edit_file`, `web_fetch`, `grep`, `glob`, `artifact_path`), read-only
  git inspection (`git_status` / `git_log` / `git_diff` / `git_blame`,
  libgit2-backed), `web_search` (Tavily / Exa / Brave / SerpAPI, auto-
  routes on configured key), `explore` (read-only in-process subagent),
  `load_skill`, a per-session todo scratchpad, and session-control
  (`compact_now`, `clear_history`).
- **Multiple providers.** Anthropic native, Anthropic-compatible
  gateways (OpenRouter / internal proxies — set `ANTHROPIC_BASE_URL`),
  Ollama (native `/api/chat` NDJSON or an Anthropic-compatible shim),
  and Codex via subprocess bridge.
- **Operator surface.** Per-group token budgets and turn-rate caps,
  sender approvals (with in-channel prompts), dead-letter inspection
  and replay, audit log of every host-side mutation, Prometheus
  metrics endpoint, log rotation, SIGHUP secret rotation,
  central-DB backup / restore.
- **Reproducible session images.** Per-agent-group image fingerprint
  over `packages_apt` + `packages_npm` + `skills` + `mcp_servers`
  triggers an automatic rebuild on config diff; a rebuild failure
  falls back to the last-known-good tag and emits a metric.
- **Conservative defaults.** Idle-stop in minutes, retry cap of three,
  most webhook channels bind `127.0.0.1` (telegram + slack default to
  `0.0.0.0` — see [`docs/webhooks-tls.md`](docs/webhooks-tls.md)),
  budgets / metrics / log-rotation all opt-in.
- **Test coverage.** ~5200 passing tests, no failing.
  `cargo clippy --workspace --all-targets -- -D warnings` clean, fmt +
  clippy + test run on Linux and macOS in CI. The replay-fixture
  harness pins the inbound-route → runner → outbound-deliver pipeline
  against byte-stable expected output for a small set of channels
  (cli, telegram, slack, discord, matrix, github, webhooks); the
  other 14 channels rely on per-adapter unit tests for now.

## What's rough

Honest list of things that exist but aren't polished:

- **No tagged release yet.** The one-line `curl | bash` install
  currently falls through to `cargo install --git`; prebuilt
  tarballs land with the first `v0.x.y` tag.
- **`mattermost`, `line`, and `webhooks` (generic) bind to an
  OS-assigned port by default** — pin a stable `port` in the channel
  config before fronting them with a reverse proxy.
- **The replay-fixture capture pipeline is design-only** —
  `docs/replay-fixtures.md` describes hand-authored fixtures; the
  `IRONCLAW_FIXTURE_CAPTURE` env var and `ironclaw fixture redact`
  subcommand named in the design doc are not implemented yet.
- **Setup's `channel` step only has an interactive pairing wizard for
  Telegram.** Slack / Discord / etc. land via post-setup
  `iclaw messaging-groups create` + `iclaw wirings create`.
- **A few `iclaw` subcommands ship without descriptive `--help`
  text** (`messaging-groups`, `wirings`, `users`, `roles`,
  `members`, `destinations`). The flags work; the help is sparse.
- **`docs/cutover.md` describes a migrator that copies only the
  central DB** — per-session DBs (history, attachments) must be
  rsynced separately if you want to preserve them across the cutover.
- **`iclaw approvals`** ships `list`, `get`, and `approve --channel
  --identity` (sender approvals); there is no generic
  `iclaw approvals approve <id>` / `deny <id>` yet for the other
  approval families (channel / install / MCP).

See [`docs/plans/`](docs/plans/) for tracked follow-ups.

---

## Install

> **Pre-1.0.** No tagged releases yet, so the install script currently
> falls through to building from source. You need the Rust toolchain
> (1.85+) installed before the one-liner below will work end-to-end.
> Prebuilt tarballs land with the first `v0.x.y` tag — see
> [`docs/release-checklist.md`](docs/release-checklist.md).

One command, on Linux or macOS:

```
curl -fsSL https://raw.githubusercontent.com/phildougherty/ironclaw/main/install.sh | bash
```

What it does:

1. Detects your platform (Linux x86_64 / aarch64, macOS arm64 / x86_64).
2. Checks for Docker or Podman (won't install one — too invasive — but
   tells you what to install).
3. Installs `ironclaw`, `iclaw`, and `ironclaw-setup` to `~/.local/bin`.
   The script tries three strategies in order: (a) prebuilt release
   tarball from GitHub Releases — 404s until the first tag; (b)
   `cargo install --git` — this is the path that works today;
   (c) from inside a checkout, `cargo install --path`.
4. Launches `ironclaw-setup` to walk provider credentials, the data
   directory, and the first channel.

Re-running is safe — it detects an existing install and offers to
upgrade, skip, or resume setup.

Useful environment overrides for `install.sh`:

```
IRONCLAW_REPO=owner/fork                 # pull from a fork
IRONCLAW_INSTALL_DIR=$HOME/.local/bin    # where binaries land
IRONCLAW_RELEASE_TAG=v0.2.0              # pin a specific release (once tags exist)
IRONCLAW_SKIP_SETUP=1                    # install binaries only
IRONCLAW_SETUP_HEADLESS=1                # pass --headless through to the wizard
```

Windows is supported via WSL2 — run the one-liner inside the WSL shell.

Three binaries land on your PATH (a fourth, `ironclaw-runner`, is
baked into the session container image, not placed on the operator's
PATH):

| Binary | Role |
| --- | --- |
| `ironclaw` | Host orchestrator. Long-running; runs the inbound router, the outbound delivery loop, the per-session container manager, and the local admin socket. |
| `iclaw` | Admin client. Talks to the host's Unix socket. Read paths are open to in-container agents; mutations are host-only. |
| `ironclaw-setup` | Interactive one-time installer. Writes `.env`, builds the container image, drops a systemd unit or launchd plist, and creates a default CLI agent group so the first chat works. |

A pre-built session container image is produced as part of setup;
rebuilds are automatic on config change.

### Manual install

Requires Rust 1.85+ (pinned by `rust-toolchain.toml`) and a container
runtime (Docker on Linux, Docker / Podman / Apple Container on
macOS — `install.sh` and the wizard's `env_check` step detect all
three).

```bash
git clone https://github.com/phildougherty/ironclaw
cd ironclaw
cargo build --release --workspace
```

The three binaries land in `target/release/`. Add them to your PATH or
run `./install.sh` from the checkout — it'll detect the local build
and install to `~/.local/bin`.

### Testing install.sh

`tests/install/test_install_sh.sh` drives the installer inside a clean
Ubuntu 24.04 container under several scenarios (missing container
runtime, dry-run platform detection, idempotent re-run). Requires
Docker (or Podman via `CONTAINER_BIN=podman`):

```bash
bash tests/install/test_install_sh.sh
```

Pass `IRONCLAW_INSTALL_TEST_RUN_BUILD=1` to also exercise the
`cargo install --path` strategy (slow — adds ~5 minutes). The CI job
at `.github/workflows/ci.yml#install-sh` only runs on PRs that touch
`install.sh`, `tests/install/**`, or the workflow itself.

---

## Quickstart

Zero to a working chat in one terminal:

```bash
ironclaw-setup                # interactive; press Enter to accept defaults
ironclaw start && iclaw chat  # background the host, drop into the REPL
```

`iclaw chat` auto-starts the host the first time you run it, so
`ironclaw start` is optional. Pass `--no-autostart` to `iclaw chat` to
keep the historic "fail loudly when the host isn't running" behaviour
for scripted use.

Other lifecycle commands:

```bash
ironclaw status               # PID, uptime, paths, active session count
ironclaw status --json        # machine-readable status
ironclaw logs -f              # tail the host log (or -n 200 for the last 200 lines)
ironclaw stop                 # graceful SIGTERM (SIGKILL after grace)
ironclaw run                  # original foreground flow (for systemd / launchd)
iclaw doctor                  # composite probe; every FAIL prints a `fix:` (non-zero exit on FAIL)
iclaw health                  # sessions, audit, dropped-messages snapshot
iclaw usage --since 24h       # per-group token rollup
iclaw audit list --since 1h   # recent mutations against the host socket
```

`ironclaw-setup` auto-creates a default `cli/stdin` messaging group
wired to an agent group named `first` with session mode `shared`, so
`iclaw chat` works on the very first start. Opt out with
`IRONCLAW_SETUP_QUICKSTART=no`.

The setup step also wires the `iclaw chat` bridge: a named pipe at
`<install_root>/chat.fifo` (read by the host) and an append-log at
`<install_root>/chat.log` (written by the host, tailed by
`iclaw chat`). The host picks both paths up automatically via
`IRONCLAW_CLI_FIFO` and `IRONCLAW_CLI_LOG` (written to the install's
`.env`). To relocate them — onto `tmpfs` for lower write latency, or
out of the install root for permissions reasons — set the env vars
explicitly:

```bash
IRONCLAW_CLI_FIFO=/run/ironclaw/chat.fifo
IRONCLAW_CLI_LOG=/var/log/ironclaw/chat.log
```

When neither var is set and `IRONCLAW_DATA_DIR` is also unset (e.g.
you ran `cargo run -p ironclaw-host run` in a checkout), the cli
channel falls back to reading/writing the host process's own
stdin / stdout — the historic developer REPL.

### Headless / scripted install

```bash
IRONCLAW_SETUP_ANTHROPIC_API_KEY=sk-ant-... ironclaw-setup --headless
```

The only required variable is the provider API key. Override any
prompt by setting the matching env var; run `ironclaw-setup
--list-steps` for the canonical step list, or `--skip-step <name>` to
defer a step (valid names come from that list — `env_check`,
`data_dir`, `central_db`, `image`, `onecli`, `auth`, `mounts`,
`service_unit`, `cli_agent`, `timezone`, `channel`, `verify`,
`quickstart_group`, `first_chat`).

| Variable | Default | Purpose |
| --- | --- | --- |
| `IRONCLAW_SETUP_ANTHROPIC_API_KEY` | _required_ | Provider API key (Anthropic or compatible). |
| `IRONCLAW_SETUP_USE_ONECLI` | `no` | Enable OneCLI credential gateway. |
| `IRONCLAW_SETUP_BUILD_IMAGE` | `yes` | Build the session container image during setup. |
| `IRONCLAW_SETUP_MOUNTS` | empty | Comma-separated host paths to bind-mount read-only into every session. |
| `IRONCLAW_SETUP_WRITE_SERVICE_UNIT` | `no` | Drop a systemd / launchd unit. |
| `IRONCLAW_SETUP_SERVICE_SCOPE` | `print` | Service install scope: `system` / `user` / `print`. See [Running as a service](#running-as-a-service). |
| `IRONCLAW_SETUP_SERVICE_ENABLE` | `yes` | When scope is not `print`, also enable + start the service. |
| `IRONCLAW_SETUP_TIMEZONE` | system | Container timezone. |
| `IRONCLAW_SETUP_FIRST_CHANNEL` | `cli` | Which channel to wire first. |
| `IRONCLAW_SETUP_TELEGRAM_BOT_TOKEN` | empty | Bot token; required when `FIRST_CHANNEL=telegram` and `--headless`. Verified via `getMe`. |
| `IRONCLAW_SETUP_TELEGRAM_CHAT_ID` | empty | Optional chat id; supplied means setup skips the `/start` polling step. |
| `IRONCLAW_SETUP_QUICKSTART` | `yes` | Auto-create the default CLI agent group + wiring. |

### Choosing a provider

At the provider-URL prompt, type `openrouter` to use OpenRouter, leave
blank or type `anthropic` for the upstream API, or paste any
Anthropic-compatible base URL verbatim — a trailing `/v1` is stripped
automatically. The provider key is then forwarded into every session
container via `ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL`.

### Wire your first channel

At the `channel` setup step, pick `cli` (default — works out of the
box) or `telegram` (the only channel with an interactive pairing
wizard today). Selecting `telegram` walks you through creating a bot
with `@BotFather`, validates the token format, calls Telegram's
`getMe` to confirm the credentials, and offers to capture the first
chat id by polling `getUpdates` for ~60 seconds while you send
`/start` to the bot. The validated `TELEGRAM_BOT_TOKEN` (and optional
`TELEGRAM_CHAT_ID`) are appended to the data-dir `.env` with `0600`
perms; tokens are never echoed in logs.

For headless installs supply the answers via env vars:

```
IRONCLAW_SETUP_FIRST_CHANNEL=telegram \
IRONCLAW_SETUP_TELEGRAM_BOT_TOKEN=123456:ABC-DEF... \
IRONCLAW_SETUP_TELEGRAM_CHAT_ID=42      # optional — skips /start poll \
ironclaw-setup --headless
```

Network reachability to `api.telegram.org` is tested at setup time (10 s
timeout); if the call fails the token is still persisted with a loud
warning so air-gapped installs aren't blocked.

Slack / Discord / other channels pair post-setup:

```bash
iclaw messaging-groups create --channel-type slack --platform-id C01XYZ
iclaw wirings create --mg <messaging-group-id> --ag <agent-group-id> --engage all
```

---

## Channels

| Channel | Ingress | Egress | Notes |
| --- | --- | --- | --- |
| `cli` | stdin / FIFO | stdout / log file | Local REPL for development. |
| `telegram` | webhook or long-poll | Bot API | Inbound attachment download (text, photo, audio, video, voice, doc) with size cap. Native `send_card` (MarkdownV2 + `inline_keyboard`); button taps round-trip as inbound chat. |
| `slack` | events API | Web API | HMAC-SHA256 signature verification, files v2 upload. Native `send_card` (Block Kit `header` / `section` / `image` / `actions`); `block_actions` interactive payloads on the same webhook path round-trip button taps as inbound chat. |
| `discord` | slim gateway | REST | Pure codec/lifecycle parsers; gateway intent `38_401`. Native `send_card` (embed + `ActionRow` buttons, chunked at 5 per row); `INTERACTION_CREATE` `MESSAGE_COMPONENT` taps round-trip as inbound chat with a fire-and-forget `DEFERRED_UPDATE_MESSAGE` ACK. |
| `matrix` | `/sync` long-poll | Client-Server REST | Threads via `m.relates_to`; alias resolution cached. |
| `teams` | change-notifications webhook | Graph REST | Validation handshake + constant-time `clientState` compare; channel-target files supported, chat-target files Unsupported (delegated-auth limit). |
| `gchat` | HTTP push | REST v1 | `cardsV2` for cards; emoji shortcode map; two-step `attachments:upload` for files. |
| `mattermost` | outgoing webhook | REST v4 | Free-reply / paid-push split; two-step file upload via `/api/v4/files`. |
| `line` | webhook (HMAC-SHA256) | Messaging API | Free reply via reply-token; paid push fallback. No edit / reaction. |
| `webex` | webhook (HMAC-SHA1 or 256, auto-detect) | REST | Body fetch via `GET /messages/{id}` because webhooks omit text. |
| `whatsapp-cloud` | webhook (Meta Cloud) | Graph | `hub.verify_token` handshake + `X-Hub-Signature-256`. |
| `signal` | signal-cli RPC | signal-cli RPC | `RpcTransport` trait — tests never spawn `signal-cli`. |
| `deltachat` | `deltachat-rpc-server` | RPC | Inbound attachments via `download_full_msg` + stat/open. Edit Unsupported (DC protocol limit). |
| `imessage` | sqlite tail + osascript | osascript | macOS-only; Cocoa-epoch detection. System actions all Unsupported (AppleScript can't reach tapbacks reliably). |
| `wechat` | webhook (Work Weixin) | REST | Hand-rolled AES-256-CBC + SHA1 over sorted concat. Edit / reaction Unsupported (platform limit). |
| `emacs` | emacsclient | emacsclient | `EmacsClient` trait — tests never spawn emacs. |
| `x` | poll (`/2/dm_events`) | v2 DMs + v2 media upload | Since-id persisted to disk. |
| `linear` | webhook (HMAC-SHA256) | GraphQL | `commentCreate` / `commentUpdate` / `reactionCreate`. Files Unsupported. |
| `github` | webhook (HMAC-SHA256) | REST | LRU dedup on `X-GitHub-Delivery`. Files Unsupported (GitHub API limit). |
| `resend` | _none_ | REST | Send-only email; no reply surface. Subject / text / html / attachments + thread headers. |
| `webhooks` | generic HMAC | _none_ | One adapter for Stripe / Grafana / Sentry / Vercel / Shopify / IoT. Inbound-only by design. |

See [`docs/channels/README.md`](docs/channels/README.md) for the
adapter-by-adapter audit (what's COMPLETE, what's PARTIAL, what's
Unsupported and why), and [`docs/adding-a-channel.md`](docs/adding-a-channel.md)
for the trait template.

---

## Agent tools

The runner inside each container exposes 36 tools to the model:

**Messaging.** `send_message`, `send_file`, `edit_message`,
`add_reaction`, `ask_user_question`, `send_card`.

**Scheduling.** `schedule_task`, `list_tasks`, `cancel_task`,
`pause_task`, `resume_task`, `update_task`. The host runs a 60-second
sweep loop that fires due tasks (cron `recurrence` expressions are
evaluated, recurring tasks re-arm, one-shots transition to
`completed`). Agents do not need to maintain a "background loop" —
the scheduler is the loop.

**Self-modification.** `install_packages` (apt / npm — image rebuilds
on next spawn), `add_mcp_server` (MCP transport registration),
`create_agent` (spin up a sibling agent group, depth-capped).

**Computer use.** `shell` (bash inside the container, 60s default
timeout, 64 KiB output cap, persistent cwd + env across calls),
`read_file` (UTF-8 lossy on bad bytes, 1 MiB cap), `write_file`
(auto-mkdir-p, create or append), `edit_file` (unique-match string-
replacement; atomic via temp + rename; preserves mode), `web_fetch`
(HTTP GET/POST, 256 KiB body cap, 30s default; HTML auto-converts to
markdown), `grep` (regex search with `.gitignore`-aware traversal,
structured `{path, line, text}` rows, default cap 100 / ceiling 1000),
`glob` (gitignore-style glob, sorted paths, default cap 1000 /
ceiling 10000), `artifact_path` (returns the host-side path of the
session bind-mount so the operator can find files the agent built).

**Git inspection.** `git_status`, `git_log`, `git_diff`, `git_blame` —
read-only structured access to a libgit2-backed repository view (no
shelling to `git`). Mutations (commit / push / branch) are
intentionally absent — hand those back to the operator.

**Web search.** `web_search` with a normalised
`{title, url, snippet, published?, score?}` shape, routing
automatically based on which key is configured: `TAVILY_API_KEY`,
`EXA_API_KEY`, `BRAVE_SEARCH_API_KEY`, or `SERPAPI_API_KEY`. Per-call
result cap 1–25 (default 10), UTF-8-safe snippet truncation at 4 KiB.

**Lightweight subagent.** `explore` opens a bounded LLM loop against
the same upstream the parent uses, with a caller-supplied `task`
string. Read-only tools by default (`grep`, `glob`, `read_file`,
`web_fetch`); hard caps of 10 turns, 200 KiB cumulative input tokens,
and a 60-second wall-clock. Returns a single summary string —
intermediate exploration never enters the parent's context. Nested
`explore` calls are refused.

**Skill loader.** `load_skill` returns a named skill's `SKILL.md`
body on demand when `IRONCLAW_SKILLS_MODE=callable`. The default mode
is `inline` (every selected skill body inlined at spawn) and is still
preferred for small skill catalogues; flip to `callable` when prompt-
token cost starts to matter.

**Per-session todos.** `todo_add`, `todo_list`, `todo_update`,
`todo_delete` back a JSON scratchpad at `/data/agent_todos.json`.
Universal — useful for any agent juggling multi-step work, not
coding-specific.

**Session control.** `compact_now` (force the runner to compact the
conversation immediately rather than waiting for the threshold) and
`clear_history` (drop conversation state without losing the session).

**Persistent memory (opt-in via `IRONCLAW_GROUPS_DIR`).** When the
host is configured with a groups dir, each agent group also gets
`<groups_dir>/<id>/memory/` bind-mounted at `/data/memory/`. Agents
read and write memory files via the existing `read_file` /
`write_file` tools — no new tool required.

Every agent also receives a universal base preamble + an
`# Environment` block (today's date, session id, agent group id,
working directory, assistant name) at the top of its system prompt,
plus an optional operator-supplied `IRONCLAW.md` briefing from the
session dir or `<groups_dir>/<id>/IRONCLAW.md`.

Per-skill `SKILL.md` prose is auto-inlined into the runner's system
prompt at spawn (default `inline` mode) so the model knows *when* to
reach for each tool. Switch to `IRONCLAW_SKILLS_MODE=callable` to swap
inlined bodies for a name+description index and have the agent
retrieve bodies on demand via `load_skill`.

---

## Operator commands

`iclaw` is the local admin client; it talks to the host's Unix
socket. The most-used commands:

```bash
iclaw                                 # no-args dashboard: groups, wirings,
                                      # sessions, recent activity, next steps
iclaw doctor                          # composite first-run / ongoing diagnostic
                                      # — non-zero exit on any FAIL
iclaw health                          # one-shot probe — session breakdown + audit + drops
iclaw chat                            # interactive REPL against the cli channel
iclaw status                          # wiring digest

iclaw groups list                     # configured agent groups
iclaw groups config get <id>          # render the merged container config
iclaw groups config edit <id>         # multi-field config edit via $EDITOR (TOML)
iclaw messaging-groups list           # channel groups (e.g. slack/C12345)
iclaw wirings list                    # which messaging group → which agent group

iclaw users list                      # known sender identities
iclaw roles grant <user> admin        # role grants on the central DB
iclaw members add <agent-group> <user>  # group membership

iclaw approvals list                  # pending approvals (all families)
iclaw approvals approve --channel telegram --identity 12345

iclaw usage --since 24h               # per-group token rollup
iclaw budgets set --agent-group-id <id> --daily-tokens 100000
iclaw budgets set --agent-group-id <id> --turns-per-minute 4

iclaw audit list --since 1h           # mutation log
iclaw schema-version                  # central-DB schema check
                                      # status: ok | pending (run `ironclaw migrate`) | future (downgrade — restore from backup)

iclaw mcp list-presets                # curated MCP servers
iclaw mcp add postgres --agent-group-id <id> --env POSTGRES_CONNECTION_STRING=postgres://localhost/mydb
iclaw groups config set-resource-limits <id> --cpus 1 --memory-mb 1024
iclaw groups config set-egress-allow <id> example.com:443

iclaw dropped-messages outbound-list --since 24h
iclaw dropped-messages replay <id>

iclaw db backup /backups/ironclaw.sqlite
iclaw db restore /backups/ironclaw.sqlite  # only when host is stopped

iclaw quickstart cli --name <name>    # one-shot: create agent group + cli wiring + start
iclaw completions <bash|zsh|fish>     # drop a completion script
```

Run any command with `--json` for machine-readable output. Read paths
are callable by in-container agents; mutations are host-only.

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

**Three invariants** the code holds the line on:

1. `inbound.db` uses `journal_mode=DELETE`. WAL's shared-memory region
   doesn't propagate across the Docker bind-mount; silent data loss
   otherwise.
2. Each SQLite file has exactly one writer process (host writes
   inbound, container writes outbound).
3. Sessions are durable; containers are ephemeral. State lives in DBs
   and the filesystem, never in process memory beyond debounce /
   inflight maps.

---

## Configuration

`.env` keys the host reads at boot. `ironclaw-setup` writes a
populated copy; production overrides go in your service unit.

| Key | Purpose |
| --- | --- |
| `ANTHROPIC_API_KEY` | Provider key forwarded into every spawned container. |
| `ANTHROPIC_BASE_URL` | Optional Anthropic-compatible base URL (OpenRouter, internal proxy). Trailing `/v1` stripped automatically. |
| `IRONCLAW_DATA_DIR` | Host data root. Setup writes per-platform install path; defaults to `./data` when unset. |
| `IRONCLAW_ICLAW_SOCKET` | Override socket path the host listens on; otherwise resolved per-platform. (The `iclaw` client reads `ICLAW_SOCKET` for the same purpose on the dial side; setup writes both into `.env`.) |
| `IRONCLAW_SKILLS_DIR` | Skills directory whose `SKILL.md` bodies get auto-inlined into the runner's system prompt at spawn (`inline` mode) or advertised as a name+description index (`callable` mode). |
| `IRONCLAW_GROUPS_DIR` | Per-agent-group override root. `<groups_dir>/<id>/skills/` shadows global skills with matching names. `<groups_dir>/<id>/memory/` is bind-mounted at `/data/memory/` so per-group memory persists across sessions. `<groups_dir>/<id>/IRONCLAW.md` is read as an operator-supplied briefing into every spawn's system prompt. |
| `IRONCLAW_SKILLS_MODE` | `inline` (default) or `callable`. Inline puts every selected skill body in the prompt at spawn. Callable emits only an index and writes a per-session `skills.json` for the `load_skill` MCP tool. |
| `IRONCLAW_METRICS_ADDR` | Bind address for the Prometheus endpoint (e.g. `127.0.0.1:9090`). Off when unset. |
| `IRONCLAW_LOG_DIR` | Enable daily-rotating file appender alongside stderr. Off when unset. |
| `IRONCLAW_DEFAULT_PROVIDER` | Provider name for sessions whose group hasn't pinned one. |
| `IRONCLAW_DEFAULT_IMAGE_TAG` | Default container image tag when no `container_configs` row pins one. |
| `IRONCLAW_CONTAINER_GPU` | Set to `1` to enable Nvidia GPU passthrough on session containers (requires nvidia-container-toolkit on the host). Off by default. |
| `TAVILY_API_KEY` / `EXA_API_KEY` / `BRAVE_SEARCH_API_KEY` / `SERPAPI_API_KEY` | Forwarded into the container so `web_search` auto-selects a backend. |
| `IRONCLAW_CODEX_BINARY` | Runner-side: absolute path to the Codex CLI inside the container. Read by the runner only when `provider == "codex"`. Defaults to `/usr/local/bin/codex`. Host forwards this through. |
| `IRONCLAW_CODEX_ARGS` | Runner-side: comma-separated extra args appended to every Codex spawn (e.g. `--json,--no-color`). Defaults to `--json`. |

A SIGHUP on the host re-reads the `.env` file, updates the forwarded
keys, and increments the `ironclaw_secrets_rotated_total` metric
counter. Running containers see the rotated values after the next
idle-stop + respawn (default 5 minutes); for an immediate rotation,
`iclaw groups restart <id>`.

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
`ironclaw_secrets_rotated_total`,
`ironclaw_budget_exhausted_total{agent_group_id, gate}`,
`ironclaw_budget_exhausted_replies_total{agent_group_id}`,
`ironclaw_budget_exhausted_suppressed_total{agent_group_id}`.

Histograms: `ironclaw_llm_call_seconds`, `ironclaw_llm_tokens_input`,
`ironclaw_llm_tokens_output`, `ironclaw_container_spawn_seconds`.

Log rotation (also opt-in):

```bash
IRONCLAW_LOG_DIR=/var/log/ironclaw ironclaw run
```

Writes one daily-rotated file alongside the stderr stream so container
output never contaminates the data path.

See [`docs/observability.md`](docs/observability.md) for the full
operator playbook.

---

## Running as a service

For local / developer installs the `ironclaw start` / `ironclaw stop`
lifecycle commands are usually enough. For server installs that need
auto-start at boot, `ironclaw-setup`'s `service_unit` step handles the
whole install end-to-end instead of just printing the unit file.

At the prompt (or via `IRONCLAW_SETUP_SERVICE_SCOPE`) pick one of:

- `system` — install to `/etc/systemd/system/ironclaw.service` (or
  `/Library/LaunchDaemons/com.ironclaw.host.plist`), then
  `systemctl daemon-reload` + `systemctl enable --now ironclaw`
  (`launchctl bootstrap system <plist>` on macOS). Requires the wizard
  to be running as root. When the wizard is not root, it falls back to
  `user` scope and prints a warning rather than prompting for the sudo
  password mid-run.
- `user` — install to `~/.config/systemd/user/ironclaw.service` (or
  `~/Library/LaunchAgents/com.ironclaw.host.plist`), then `systemctl
  --user enable --now` (`launchctl bootstrap gui/<uid>` on macOS). No
  privilege elevation needed.
- `print` — write the unit to the per-user default path and print the
  enable command. Default for headless installs so unattended pipelines
  don't change shape unless they opt in.

After enabling, setup polls the `iclaw.sock` admin socket for ~10s and
prints either `ironclaw service is running, socket at <path>` or
`service didn't come up — check journalctl -u ironclaw` (or
`launchctl print gui/<uid>/com.ironclaw.host` on macOS). Re-running
setup with the same scope is idempotent: if the on-disk unit already
matches the generated body, the step is a no-op.

The `--generate-unit <systemd|launchd>` flag still works for operators
who want to render a unit to stdout / a file for their
config-management tool without going through the full wizard.

---

## Agent skills

Skills under `skills/` are markdown bundles auto-discovered by
`ironclaw-skills` and either inlined into the running agent's system
prompt (default `inline` mode) or advertised as a compact
name+description index and served on demand via the `load_skill` tool
(`IRONCLAW_SKILLS_MODE=callable`). Capability docs (`send-message`,
`install-packages`, ...) describe the in-tree MCP tools; guided-flow
skills describe a multi-turn interaction the agent runs with the
user:

- `skills/customize/` — change the model, install a package or MCP
  server, edit the per-group behavior prompt, raise/lower the
  daily-token budget. The agent prints the exact `iclaw` command for
  any host-only mutation.
- `skills/debug/` — triage a "you didn't reply" / "it's slow" report:
  pull what's reachable from inside the container, then hand
  `iclaw health`, `iclaw audit list`, and `iclaw dropped-messages list`
  to the operator.
- `skills/todo-tracker/`, `skills/agent-memory/` — universal
  scratchpad and persistent-memory disciplines for any agent.
- `skills/coding-task/`, `skills/git-commit/`, `skills/code-review/`,
  `skills/testing/` — bundle for agents doing coding work. Off by
  default — flip on per agent group with `iclaw groups enable-coding
  <id>`, off again with `iclaw groups disable-coding <id>`.

Drop a new directory with a `SKILL.md` (YAML frontmatter + markdown
body) into `skills/` and the next container boot picks it up — no
registry edits required.

---

## Documentation

Operator-facing guides:

- [`docs/channels/README.md`](docs/channels/README.md) — adapter audit
  (what's COMPLETE, what's PARTIAL, what's Unsupported and why).
- [`docs/adding-a-channel.md`](docs/adding-a-channel.md) — build a new
  channel adapter from the trait template.
- [`docs/container-config.md`](docs/container-config.md) — per-group
  image rebuild, egress allow-list, resource caps.
- [`docs/observability.md`](docs/observability.md) — metrics endpoint
  and log rotation.
- [`docs/db-backup.md`](docs/db-backup.md) — backup and restore the
  central SQLite database.
- [`docs/web-search.md`](docs/web-search.md) — the multi-provider
  `web_search` tool.
- [`docs/webhooks-tls.md`](docs/webhooks-tls.md) — TLS termination via
  Caddy / nginx / Cloudflare Tunnel, per-channel default ports.
- [`docs/cutover.md`](docs/cutover.md) — migrate from a predecessor
  installation onto Ironclaw.
- [`docs/replay-fixtures.md`](docs/replay-fixtures.md) — the
  differential-replay test harness.
- [`docs/release-checklist.md`](docs/release-checklist.md) — steps for
  cutting a release.

---

## Status

Pre-1.0. The end-to-end chat path works against any
Anthropic-API-compatible provider. The operator surface is shaped for
production use (audit log, budgets, doctor probe, schema-version
guard, dead-letter replay, metrics endpoint) but has not been
hardened against any specific production deployment.

What's solid: the inbound-route → runner → outbound-deliver pipeline,
covered by ~5200 passing tests and a replay-fixture harness against
byte-stable expected output (for 7 of the 21 channels — the other 14
rely on unit tests). The 36-tool MCP surface has a coverage test
asserting every registered tool is mentioned in at least one skill.

What's not solid yet: the [What's rough](#whats-rough) list above.
Tracked follow-ups live in [`docs/plans/`](docs/plans/).

Contributions welcome. The repo has a strong "no half-finished things
in tree" preference; new channels and tools should ship complete
(including the `Unsupported` returns for what they don't do) — see
[`docs/adding-a-channel.md`](docs/adding-a-channel.md) for the
contract.

---

## License

MIT — see [`LICENSE`](LICENSE).
