# Copperclaw

A self-hosted runtime for Claude-style agents. Each session runs in
its own Linux container; the host wires 21 messaging-channel adapters
into a router on the inbound side and a delivery loop on the outbound
side. An admin client (`cclaw`) and a setup wizard (`copperclaw-setup`)
live alongside the host binary (`copperclaw`).

Written in Rust. Pre-1.0. Prebuilt binaries for Linux and macOS ship
with each tagged release; `install.sh` fetches the latest tarball and
falls back to building from source. Works end-to-end against any
Anthropic-API-compatible provider. Rough edges noted in
[What's rough](#whats-rough).

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
- **51 in-tree tools the model can call** (plus an opt-in interactive
  browser and three host-brokered preview verbs), grouped: messaging
  (`send_message` / `send_file` / `edit_message` / `add_reaction` /
  `ask_user_question` / `send_card`), scheduling (`schedule_task` and
  five companions — backed by a real cron-evaluating sweep loop in
  the host), delegation (`delegate` / `delegate_batch` write-capable
  build workers, `explore` read-only subagent, `create_agent`),
  self-modification (`install_packages`, `add_mcp_server`,
  `save_skill`), computer use (`shell`, `read_file`, `write_file`,
  `edit_file`, `multi_edit`, `apply_patch`, `copy_file`, `web_fetch`,
  `grep`, `glob`, `artifact_path`), read-only git inspection
  (`git_status` / `git_log` / `git_diff` / `git_blame`,
  libgit2-backed), `web_search` (Tavily / Exa / Brave / SerpAPI, auto-
  routes on configured key), vision + browser (`view_image`,
  `browser_render`, `ui_screenshot`, `ui_inspect`), code quality
  (`diagnostics`, `self_review`), web preview (`expose_preview` /
  `close_preview` / `make_preview_public`), persistent memory
  (`memory_save` / `memory_search` / `memory_get`), `load_skill`, a
  per-session todo scratchpad, and session-control (`compact_now`,
  `clear_history`). See [Agent tools](#agent-tools) below.
- **Watchable builds.** Long tasks drive a self-editing task HUD
  (step counter, todo state, blocker cards) instead of dead air, and
  final answers reveal progressively. Finished web apps are served to
  the user via `expose_preview` — optionally through a public tunnel —
  and the agent screenshots its own UI (`ui_screenshot` /
  `ui_inspect`) and fixes what it sees before an enforced self-review
  gate signs off on delivery.
- **Multiple providers, with failover.** Anthropic native (with
  prompt caching), Anthropic-compatible gateways (OpenRouter /
  internal proxies — set `ANTHROPIC_BASE_URL`), Ollama (native
  `/api/chat` NDJSON or an Anthropic-compatible shim), and Codex or
  OpenCode via subprocess bridges. An ordered fallback chain tracks
  per-provider health (rate-limit cooldowns, down detection, re-probe)
  and rotates across multiple keys, including mid-session failover.
- **Hardened container boundary.** A tool policy engine with a
  provenance/taint gate on untrusted input, opt-in deny-default
  egress with per-group allow-lists, per-group mention gating and DM
  pairing, and a credential broker that keeps long-lived secrets out
  of the container environment.
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
- **Test coverage.** ~7,700 passing tests, no failing.
  `cargo clippy --workspace --all-targets -- -D warnings` clean, fmt +
  clippy + test run on Linux and macOS in CI. The replay-fixture
  harness pins the inbound-route → runner → outbound-deliver pipeline
  against byte-stable expected output for 11 of the 21 channels
  (cli, telegram, slack, discord, matrix, teams, gchat, signal,
  deltachat, github, webhooks); the other 10 rely on per-adapter
  unit tests for now.

## What's rough

Honest list of things that exist but aren't polished:

- **`mattermost`, `line`, and `webhooks` (generic) bind to an
  OS-assigned port by default** — pin a stable `port` in the channel
  config before fronting them with a reverse proxy.
- **The replay-fixture capture pipeline is design-only** —
  `docs/replay-fixtures.md` describes hand-authored fixtures; the
  `COPPERCLAW_FIXTURE_CAPTURE` env var and `copperclaw fixture redact`
  subcommand named in the design doc are not implemented yet.
- **Setup's `channel` step only has an interactive pairing wizard for
  Telegram.** Slack / Discord / etc. land via post-setup
  `cclaw messaging-groups create` + `cclaw wirings create`.
- **`docs/cutover.md` describes a migrator that copies only the
  central DB** — per-session DBs (history, attachments) must be
  rsynced separately if you want to preserve them across the cutover.

See [`docs/plans/`](docs/plans/) for tracked follow-ups.

---

## Install

> **Pre-1.0.** Prebuilt tarballs are published with each `v0.x.y` tag
> on the [Releases page](https://github.com/phildougherty/copperclaw/releases);
> the install script fetches the latest one for your platform. If no
> tarball matches (or you're on a fork without releases), it falls back
> to building from source, which needs the Rust toolchain (1.85+).

One command, on Linux or macOS:

```
curl -fsSL https://raw.githubusercontent.com/phildougherty/copperclaw/main/install.sh | bash
```

What it does:

1. Detects your platform (Linux x86_64 / aarch64, macOS arm64 / x86_64).
2. Checks for Docker or Podman (won't install one — too invasive — but
   tells you what to install).
3. Installs `copperclaw`, `cclaw`, and `copperclaw-setup` to `~/.local/bin`.
   The script tries three strategies in order: (a) prebuilt release
   tarball from GitHub Releases; (b) `cargo install --git` (needs the
   Rust toolchain); (c) from inside a checkout, `cargo install --path`.
4. Launches `copperclaw-setup` to walk provider credentials, the data
   directory, and the first channel.

Re-running is safe — it detects an existing install and offers to
upgrade, skip, or resume setup.

Useful environment overrides for `install.sh`:

```
COPPERCLAW_REPO=owner/fork                 # pull from a fork
COPPERCLAW_INSTALL_DIR=$HOME/.local/bin    # where binaries land
COPPERCLAW_RELEASE_TAG=v0.2.0              # pin a specific release (once tags exist)
COPPERCLAW_SKIP_SETUP=1                    # install binaries only
COPPERCLAW_SETUP_HEADLESS=1                # pass --headless through to the wizard
```

Windows is supported via WSL2 — run the one-liner inside the WSL shell.

Three binaries land on your PATH (a fourth, `copperclaw-runner`, is
baked into the session container image, not placed on the operator's
PATH):

| Binary | Role |
| --- | --- |
| `copperclaw` | Host orchestrator. Long-running; runs the inbound router, the outbound delivery loop, the per-session container manager, and the local admin socket. |
| `cclaw` | Admin client. Talks to the host's Unix socket. Read paths are open to in-container agents; mutations are host-only. |
| `copperclaw-setup` | Interactive one-time installer. Writes `.env`, builds the container image, drops a systemd unit or launchd plist, and creates a default CLI agent group so the first chat works. |

A pre-built session container image is produced as part of setup;
rebuilds are automatic on config change.

### Manual install

Requires Rust 1.85+ (pinned by `rust-toolchain.toml`) and a container
runtime (Docker on Linux, Docker / Podman / Apple Container on
macOS — `install.sh` and the wizard's `env_check` step detect all
three).

```bash
git clone https://github.com/phildougherty/copperclaw
cd copperclaw
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

Pass `COPPERCLAW_INSTALL_TEST_RUN_BUILD=1` to also exercise the
`cargo install --path` strategy (slow — adds ~5 minutes). The CI job
at `.github/workflows/ci.yml#install-sh` only runs on PRs that touch
`install.sh`, `tests/install/**`, or the workflow itself.

---

## Quickstart

Zero to a working chat in one terminal:

```bash
copperclaw-setup                # interactive; press Enter to accept defaults
copperclaw start && cclaw chat  # background the host, drop into the REPL
```

`cclaw chat` auto-starts the host the first time you run it, so
`copperclaw start` is optional. Pass `--no-autostart` to `cclaw chat` to
keep the historic "fail loudly when the host isn't running" behaviour
for scripted use.

Other lifecycle commands:

```bash
copperclaw status               # PID, uptime, paths, active session count
copperclaw status --json        # machine-readable status
copperclaw logs -f              # tail the host log (or -n 200 for the last 200 lines)
copperclaw stop                 # graceful SIGTERM (SIGKILL after grace)
copperclaw run                  # original foreground flow (for systemd / launchd)
cclaw doctor                  # composite probe; every FAIL prints a `fix:` (non-zero exit on FAIL)
cclaw health                  # sessions, audit, dropped-messages snapshot
cclaw usage --since 24h       # per-group token rollup
cclaw audit list --since 1h   # recent mutations against the host socket
```

`copperclaw-setup` auto-creates a default `cli/stdin` messaging group
wired to an agent group named `first` with session mode `shared`, so
`cclaw chat` works on the very first start. Opt out with
`COPPERCLAW_SETUP_QUICKSTART=no`.

The setup step also wires the `cclaw chat` bridge: a named pipe at
`<install_root>/chat.fifo` (read by the host) and an append-log at
`<install_root>/chat.log` (written by the host, tailed by
`cclaw chat`). The host picks both paths up automatically via
`COPPERCLAW_CLI_FIFO` and `COPPERCLAW_CLI_LOG` (written to the install's
`.env`). To relocate them — onto `tmpfs` for lower write latency, or
out of the install root for permissions reasons — set the env vars
explicitly:

```bash
COPPERCLAW_CLI_FIFO=/run/copperclaw/chat.fifo
COPPERCLAW_CLI_LOG=/var/log/copperclaw/chat.log
```

When neither var is set and `COPPERCLAW_DATA_DIR` is also unset (e.g.
you ran `cargo run -p copperclaw-host run` in a checkout), the cli
channel falls back to reading/writing the host process's own
stdin / stdout — the historic developer REPL.

### Headless / scripted install

```bash
COPPERCLAW_SETUP_ANTHROPIC_API_KEY=sk-ant-... copperclaw-setup --headless
```

The only required variable is the provider API key. Override any
prompt by setting the matching env var; run `copperclaw-setup
--list-steps` for the canonical step list, or `--skip-step <name>` to
defer a step (valid names come from that list — `env_check`,
`data_dir`, `central_db`, `image`, `onecli`, `auth`, `mounts`,
`service_unit`, `cli_agent`, `timezone`, `channel`, `verify`,
`quickstart_group`, `first_chat`).

| Variable | Default | Purpose |
| --- | --- | --- |
| `COPPERCLAW_SETUP_ANTHROPIC_API_KEY` | _required_ | Provider API key (Anthropic or compatible). |
| `COPPERCLAW_SETUP_USE_ONECLI` | `no` | Enable OneCLI credential gateway. |
| `COPPERCLAW_SETUP_BUILD_IMAGE` | `yes` | Build the session container image during setup. |
| `COPPERCLAW_SETUP_MOUNTS` | empty | Comma-separated host paths to bind-mount read-only into every session. |
| `COPPERCLAW_SETUP_WRITE_SERVICE_UNIT` | `no` | Drop a systemd / launchd unit. |
| `COPPERCLAW_SETUP_SERVICE_SCOPE` | `print` | Service install scope: `system` / `user` / `print`. See [Running as a service](#running-as-a-service). |
| `COPPERCLAW_SETUP_SERVICE_ENABLE` | `yes` | When scope is not `print`, also enable + start the service. |
| `COPPERCLAW_SETUP_TIMEZONE` | system | Container timezone. |
| `COPPERCLAW_SETUP_FIRST_CHANNEL` | `cli` | Which channel to wire first. |
| `COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN` | empty | Bot token; required when `FIRST_CHANNEL=telegram` and `--headless`. Verified via `getMe`. |
| `COPPERCLAW_SETUP_TELEGRAM_CHAT_ID` | empty | Optional chat id; supplied means setup skips the `/start` polling step. |
| `COPPERCLAW_SETUP_QUICKSTART` | `yes` | Auto-create the default CLI agent group + wiring. |

### Choosing a provider

At the provider-URL prompt, type `openrouter` to use OpenRouter, leave
blank or type `anthropic` for the upstream API, or paste any
Anthropic-compatible base URL verbatim — a trailing `/v1` is stripped
automatically. The provider key is then forwarded into every session
container via `ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL`.

### Wire your first channel

At the `channel` setup step, pick `cli` (default — works out of the
box) or `telegram` (the only channel with an interactive pairing
wizard today; `slack` / `discord` are accepted as choices but
currently just point you at the post-setup pairing commands below). Selecting `telegram` walks you through creating a bot
with `@BotFather`, validates the token format, calls Telegram's
`getMe` to confirm the credentials, and offers to capture the first
chat id by polling `getUpdates` for ~60 seconds while you send
`/start` to the bot. The validated `TELEGRAM_BOT_TOKEN` (and optional
`TELEGRAM_CHAT_ID`) are appended to the data-dir `.env` with `0600`
perms; tokens are never echoed in logs.

For headless installs supply the answers via env vars:

```
COPPERCLAW_SETUP_FIRST_CHANNEL=telegram \
COPPERCLAW_SETUP_TELEGRAM_BOT_TOKEN=123456:ABC-DEF... \
COPPERCLAW_SETUP_TELEGRAM_CHAT_ID=42      # optional — skips /start poll \
copperclaw-setup --headless
```

Network reachability to `api.telegram.org` is tested at setup time (10 s
timeout); if the call fails the token is still persisted with a loud
warning so air-gapped installs aren't blocked.

Slack / Discord / other channels pair post-setup:

```bash
cclaw messaging-groups create --channel-type slack --platform-id C01XYZ
cclaw wirings create --mg <messaging-group-id> --ag <agent-group-id> --engage all
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

The runner inside each container exposes 51 tools to the model, plus
an opt-in interactive browser and three host-brokered preview verbs:

**Messaging.** `send_message`, `send_file`, `edit_message`,
`add_reaction`, `ask_user_question`, `send_card`.

**Scheduling.** `schedule_task`, `list_tasks`, `cancel_task`,
`pause_task`, `resume_task`, `update_task`. The host runs a 60-second
sweep loop that fires due tasks (cron `recurrence` expressions are
evaluated, recurring tasks re-arm, one-shots transition to
`completed`). Agents do not need to maintain a "background loop" —
the scheduler is the loop.

**Delegation.** `delegate` (a write-capable middle-tier build worker
for a scoped subtask), `delegate_batch` (parallel fan-out of delegate
workers with a post-join integration verify), `create_agent` (spin up
a sibling agent group, depth-capped), and `explore` (see below).

**Self-modification.** `install_packages` (apt / npm — installed
session-locally so it works *this* turn; the image rebuilds on next
spawn), `add_mcp_server` (MCP transport registration), `save_skill`
(agent-authored persistent skills that survive the session).

**Computer use.** `shell` (bash inside the container, 60s default
timeout, 64 KiB output cap, persistent cwd + env across calls),
`read_file` (UTF-8 lossy on bad bytes, 1 MiB cap), `write_file`
(auto-mkdir-p, create or append), `edit_file` (unique-match string-
replacement; atomic via temp + rename; preserves mode), `multi_edit`
(several replacements in one call), `apply_patch` (unified-diff
application), `copy_file`, `web_fetch` (HTTP GET/POST, 256 KiB body
cap, 30s default; HTML auto-converts to markdown), `grep` (regex
search with `.gitignore`-aware traversal, structured
`{path, line, text}` rows, default cap 100 / ceiling 1000), `glob`
(gitignore-style glob, sorted paths, default cap 1000 / ceiling
10000), `artifact_path` (returns the host-side path of the session
bind-mount so the operator can find files the agent built).

**Vision + browser.** `view_image` (put an image from disk in front
of the model), `browser_render` (headless read-only render of a URL
via the bundled Chromium), `ui_screenshot` (screenshot the agent's
own running app, with viewport / format / quality control),
`ui_inspect` (console errors + element geometry from the running
page — the "see → fix" loop for web builds). An interactive browser
(`browser_interact`) is opt-in via `COPPERCLAW_BROWSER_ENABLED` +
`COPPERCLAW_BROWSER_INTERACTIVE`.

**Code quality.** `diagnostics` (structured lint / typecheck output)
and `self_review` — an enforced review gate the runner requires
before final delivery on coding tasks.

**Web preview.** `expose_preview`, `close_preview`,
`make_preview_public` — host-brokered verbs that serve an app running
in the session container to the user, optionally through a public
tunnel (`COPPERCLAW_PUBLIC_TUNNEL_ENABLED`).

**Git inspection.** `git_status`, `git_log`, `git_diff`, `git_blame` —
read-only structured access to a libgit2-backed repository view (no
shelling to `git`). Mutations (commit / push / branch) are
intentionally absent — hand those back to the operator.

**Web search.** `web_search` with a normalised
`{title, url, snippet, published?, score?}` shape, routing
automatically based on which key is configured: `TAVILY_API_KEY`,
`EXA_API_KEY`, `BRAVE_SEARCH_API_KEY`, or `SERPAPI_API_KEY` (pin one
with `COPPERCLAW_WEB_SEARCH_PROVIDER`). Per-call result cap 1–25
(default 10), UTF-8-safe snippet truncation at 4 KiB.

**Lightweight subagent.** `explore` opens a bounded LLM loop against
the same upstream the parent uses, with a caller-supplied `task`
string. Read-only tools by default (`grep`, `glob`, `read_file`,
`web_fetch`); hard caps of 10 turns, 200 KiB cumulative input tokens,
and a 60-second wall-clock. Returns a single summary string —
intermediate exploration never enters the parent's context. Nested
`explore` calls are refused.

**Skill loader.** `load_skill` returns a named skill's `SKILL.md`
body on demand when `COPPERCLAW_SKILLS_MODE=callable`. The default mode
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

**Persistent memory.** `memory_save`, `memory_search`, `memory_get` —
cross-session memory backed by the per-group memory dir
(`<groups_dir>/<id>/memory/`, bind-mounted at `/data/memory/` when
`COPPERCLAW_GROUPS_DIR` is configured). The memory files stay plain
markdown on disk, so they're also reachable through the ordinary
`read_file` / `write_file` tools.

Every agent also receives a universal base preamble + an
`# Environment` block (today's date, session id, agent group id,
working directory, assistant name) at the top of its system prompt,
plus an optional operator-supplied `COPPERCLAW.md` briefing from the
session dir or `<groups_dir>/<id>/COPPERCLAW.md`.

Per-skill `SKILL.md` prose is auto-inlined into the runner's system
prompt at spawn (default `inline` mode) so the model knows *when* to
reach for each tool. Switch to `COPPERCLAW_SKILLS_MODE=callable` to swap
inlined bodies for a name+description index and have the agent
retrieve bodies on demand via `load_skill`.

---

## Operator commands

`cclaw` is the local admin client; it talks to the host's Unix
socket. The most-used commands:

```bash
cclaw                                 # no-args dashboard: groups, wirings,
                                      # sessions, recent activity, next steps
cclaw doctor                          # composite first-run / ongoing diagnostic
                                      # — non-zero exit on any FAIL
cclaw health                          # one-shot probe — session breakdown + audit + drops
cclaw chat                            # interactive REPL against the cli channel
cclaw status                          # wiring digest

cclaw groups list                     # configured agent groups
cclaw groups config get <id>          # render the merged container config
cclaw groups config edit <id>         # multi-field config edit via $EDITOR (TOML)
cclaw messaging-groups list           # channel groups (e.g. slack/C12345)
cclaw wirings list                    # which messaging group → which agent group

cclaw users list                      # known sender identities
cclaw roles grant <user> admin        # role grants on the central DB
cclaw members add <agent-group> <user>  # group membership

cclaw approvals list                  # pending approvals (all families)
cclaw approvals approve-id <id>       # approve any family by row id
cclaw approvals deny <id>
cclaw approvals approve --channel telegram --identity 12345   # sender approvals

cclaw usage --since 24h               # per-group token rollup
cclaw budgets list                    # caps + today's spend + breach state
cclaw budgets set --agent-group-id <id> --daily-tokens 100000
cclaw budgets set --agent-group-id <id> --daily-cost 2.50   # USD/day; counts priced models only
cclaw budgets set --agent-group-id <id> --turns-per-minute 4

cclaw audit list --since 1h           # mutation log
cclaw schema-version                  # central-DB schema check
                                      # status: ok | pending (run `copperclaw migrate`) | future (downgrade — restore from backup)

cclaw mcp list-presets                # curated MCP servers
cclaw mcp add postgres --agent-group-id <id> --env POSTGRES_CONNECTION_STRING=postgres://localhost/mydb
cclaw groups config set-resource-limits <id> --cpus 1 --memory-mb 1024
cclaw groups config set-egress-allow <id> example.com:443

cclaw dropped-messages outbound-list --since 24h
cclaw dropped-messages replay <id>

cclaw db backup /backups/copperclaw.sqlite
cclaw db restore /backups/copperclaw.sqlite  # only when host is stopped

cclaw quickstart cli --name <name>    # one-shot: create agent group + cli wiring + start
cclaw completions <bash|zsh|fish>     # drop a completion script
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
  - cclaw socket server   (Unix socket; newline-delimited JSON)
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

`.env` keys the host reads at boot. `copperclaw-setup` writes a
populated copy; production overrides go in your service unit.

| Key | Purpose |
| --- | --- |
| `ANTHROPIC_API_KEY` | Provider key forwarded into every spawned container. |
| `ANTHROPIC_BASE_URL` | Optional Anthropic-compatible base URL (OpenRouter, internal proxy). Trailing `/v1` stripped automatically. |
| `COPPERCLAW_DATA_DIR` | Host data root. Setup writes per-platform install path; defaults to `./data` when unset. |
| `COPPERCLAW_CCLAW_SOCKET` | Override socket path the host listens on; otherwise resolved per-platform. (The `cclaw` client reads `CCLAW_SOCKET` for the same purpose on the dial side; setup writes both into `.env`.) |
| `COPPERCLAW_SKILLS_DIR` | Skills directory whose `SKILL.md` bodies get auto-inlined into the runner's system prompt at spawn (`inline` mode) or advertised as a name+description index (`callable` mode). |
| `COPPERCLAW_GROUPS_DIR` | Per-agent-group override root. `<groups_dir>/<id>/skills/` shadows global skills with matching names. `<groups_dir>/<id>/memory/` is bind-mounted at `/data/memory/` so per-group memory persists across sessions. `<groups_dir>/<id>/COPPERCLAW.md` is read as an operator-supplied briefing into every spawn's system prompt. |
| `COPPERCLAW_SKILLS_MODE` | `inline` (default) or `callable`. Inline puts every selected skill body in the prompt at spawn. Callable emits only an index and writes a per-session `skills.json` for the `load_skill` MCP tool. |
| `COPPERCLAW_METRICS_ADDR` | Bind address for the Prometheus endpoint (e.g. `127.0.0.1:9090`). Off when unset. |
| `COPPERCLAW_LOG_DIR` | Enable daily-rotating file appender alongside stderr. Off when unset. |
| `COPPERCLAW_DEFAULT_PROVIDER` | Provider name for sessions whose group hasn't pinned one. |
| `COPPERCLAW_DEFAULT_IMAGE_TAG` | Default container image tag when no `container_configs` row pins one. |
| `COPPERCLAW_CONTAINER_GPU` | Set to `1` to enable Nvidia GPU passthrough on session containers (requires nvidia-container-toolkit on the host). Off by default. |
| `COPPERCLAW_DEFAULT_MODEL` / `COPPERCLAW_DEFAULT_TEMPERATURE` | Model + sampling defaults for groups that don't pin their own (~0.3 steadies tool-calling on small local models). |
| `COPPERCLAW_MAX_TASK_TOKENS` | Per-task token budget enforced by the runner. |
| `COPPERCLAW_EGRESS_MODE` | Container egress posture — set to enable deny-default egress with per-group allow-lists. |
| `COPPERCLAW_BROWSER_ENABLED` / `COPPERCLAW_BROWSER_INTERACTIVE` | Opt in to the interactive browser tool (`browser_interact`). |
| `COPPERCLAW_PUBLIC_TUNNEL_ENABLED` | Allow `make_preview_public` to open a public tunnel to a session preview. |
| `COPPERCLAW_HUD_MODE` | Task-HUD display mode for long-running tasks. |
| `COPPERCLAW_WEB_SEARCH_PROVIDER` | Pin a `web_search` backend instead of key-based auto-routing. |
| `COPPERCLAW_REQUIRE_MENTION_GROUPS` / `COPPERCLAW_REQUIRE_MENTION_DMS` | Mention gating for group chats / DMs (defaults: required in groups, off in DMs). |
| `TAVILY_API_KEY` / `EXA_API_KEY` / `BRAVE_SEARCH_API_KEY` / `SERPAPI_API_KEY` | Forwarded into the container so `web_search` auto-selects a backend. |
| `COPPERCLAW_CODEX_BINARY` | Runner-side: absolute path to the Codex CLI inside the container. Read by the runner only when `provider == "codex"`. Defaults to `/usr/local/bin/codex`. Host forwards this through. |
| `COPPERCLAW_CODEX_ARGS` | Runner-side: comma-separated extra args appended to every Codex spawn (e.g. `--json,--no-color`). Defaults to `--json`. |
| `COPPERCLAW_DEFAULT_EFFORT` | Reasoning-effort tier (`low` / `medium` / `high`, case-insensitive). `low` and `high` are sent to providers that support it; `medium` (the default) sends no effort field, leaving the model's own default — as does unset or unrecognised. |
| `COPPERCLAW_CREDENTIAL_BROKER` | Truthy (`1`/`true`/`yes`/`on`/`enable`) turns on the in-host credential broker: containers receive short-lived broker tokens instead of the raw `ANTHROPIC_API_KEY`, so a compromised container can't exfiltrate the master key. Strictly opt-in; off by default. |
| `COPPERCLAW_BROKER_TOKEN_TTL_SECS` | Override the broker token lifetime (seconds). Only read when the credential broker is enabled. |
| `COPPERCLAW_EXPECTED_IMAGE_DIGEST` | Pin the expected session-image content digest (`sha256:<hex>` or bare hex) for the boot-time attestation check. Unset means no baseline — the check reports `no-baseline` and changes nothing. |
| `COPPERCLAW_TODO_NOTIFICATIONS` | Set to `1` to enable host-side notifications when an agent's todo list changes. Default off. |

The table lists the keys most installs touch — it is not exhaustive
(runner deadlines, compaction thresholds, breadcrumb styling, and
other tuning knobs live in the source next to their subsystems).

A SIGHUP on the host re-reads the `.env` file, updates the forwarded
keys, and increments the `copperclaw_secrets_rotated_total` metric
counter. Running containers see the rotated values after the next
idle-stop + respawn (default 5 minutes); for an immediate rotation,
`cclaw groups restart <id>`.

---

## Observability

Opt-in Prometheus endpoint:

```bash
COPPERCLAW_METRICS_ADDR=127.0.0.1:9090 copperclaw run
```

The endpoint exports ~130 metric families covering the whole
pipeline: message in/out counts, container lifecycle and spawn
timing, LLM latency and token histograms, delivery failures, budget
gates, provider failover, delegation, task-HUD edits, previews and
tunnels, browser / vision tool activity, verify and self-review
gates, compaction, and egress / credential-broker activity.
Headliners: `copperclaw_messages_inbound_total` /
`_outbound_total`, `copperclaw_containers_spawned_total` /
`_crashed_total`, `copperclaw_delivery_failed_total`,
`copperclaw_llm_call_seconds`, `copperclaw_llm_tokens_input` /
`_output`, `copperclaw_budget_exhausted_total`,
`copperclaw_provider_failover_total`.

Log rotation (also opt-in):

```bash
COPPERCLAW_LOG_DIR=/var/log/copperclaw copperclaw run
```

Writes one daily-rotated file alongside the stderr stream so container
output never contaminates the data path.

See [`docs/observability.md`](docs/observability.md) for the full
operator playbook.

---

## Running as a service

For local / developer installs the `copperclaw start` / `copperclaw stop`
lifecycle commands are usually enough. For server installs that need
auto-start at boot, `copperclaw-setup`'s `service_unit` step handles the
whole install end-to-end instead of just printing the unit file.

At the prompt (or via `COPPERCLAW_SETUP_SERVICE_SCOPE`) pick one of:

- `system` — install to `/etc/systemd/system/copperclaw.service` (or
  `/Library/LaunchDaemons/com.copperclaw.host.plist`), then
  `systemctl daemon-reload` + `systemctl enable --now copperclaw`
  (`launchctl bootstrap system <plist>` on macOS). Requires the wizard
  to be running as root. When the wizard is not root, it falls back to
  `user` scope and prints a warning rather than prompting for the sudo
  password mid-run.
- `user` — install to `~/.config/systemd/user/copperclaw.service` (or
  `~/Library/LaunchAgents/com.copperclaw.host.plist`), then `systemctl
  --user enable --now` (`launchctl bootstrap gui/<uid>` on macOS). No
  privilege elevation needed.
- `print` — write the unit to the per-user default path and print the
  enable command. Default for headless installs so unattended pipelines
  don't change shape unless they opt in.

After enabling, setup polls the `cclaw.sock` admin socket for ~10s and
prints either `copperclaw service is running, socket at <path>` or
`service didn't come up — check journalctl -u copperclaw` (or
`launchctl print gui/<uid>/com.copperclaw.host` on macOS). Re-running
setup with the same scope is idempotent: if the on-disk unit already
matches the generated body, the step is a no-op.

The `--generate-unit <systemd|launchd>` flag still works for operators
who want to render a unit to stdout / a file for their
config-management tool without going through the full wizard.

---

## Agent skills

Skills under `skills/` are markdown bundles auto-discovered by
`copperclaw-skills` and either inlined into the running agent's system
prompt (default `inline` mode) or advertised as a compact
name+description index and served on demand via the `load_skill` tool
(`COPPERCLAW_SKILLS_MODE=callable`). Capability docs (`send-message`,
`install-packages`, ...) describe the in-tree MCP tools; guided-flow
skills describe a multi-turn interaction the agent runs with the
user:

- `skills/customize/` — change the model, install a package or MCP
  server, edit the per-group behavior prompt, raise/lower the
  daily-token budget. The agent prints the exact `cclaw` command for
  any host-only mutation.
- `skills/debug/` — triage a "you didn't reply" / "it's slow" report:
  pull what's reachable from inside the container, then hand
  `cclaw health`, `cclaw audit list`, and `cclaw dropped-messages list`
  to the operator.
- `skills/todo-tracker/`, `skills/agent-memory/` — universal
  scratchpad and persistent-memory disciplines for any agent.
- `skills/coding-task/`, `skills/git-commit/`, `skills/code-review/`,
  `skills/testing/` — bundle for agents doing coding work. Off by
  default — flip on per agent group with `cclaw groups enable-coding
  <id>`, off again with `cclaw groups disable-coding <id>`.
- `skills/frontend-design/`, `skills/web-app-scaffold/`,
  `skills/native-ui/`, `skills/preview/` — design critique, prototype
  scaffolding, native-card and web-preview guidance for the
  "build me X" flow.

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
  installation onto Copperclaw.
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
covered by ~7,700 passing tests and a replay-fixture harness against
byte-stable expected output (for 11 of the 21 channels — the other 10
rely on unit tests). The 51-tool MCP surface has a coverage test
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
