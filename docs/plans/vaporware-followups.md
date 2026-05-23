# Vaporware follow-ups

Open punch list of things the docs (or the operator surface) reference
that don't fully exist in code yet. Written during the doc-vs-reality
reconciliation pass on 2026-05-23.

Each item is sized: **small** (<half a day), **medium** (~day),
**large** (multi-day, design surface). For each I name the
load-bearing question — the thing a future contributor needs to
decide before writing code.

---

## Small

### `ironclaw fixture redact <dir>` subcommand

Doc claim now removed from `docs/replay-fixtures.md`, but the
underlying library at `crates/ironclaw-host/src/fixture/redact.rs`
doesn't actually exist either. The doc previously promised both a
capture pipeline (`IRONCLAW_FIXTURE_CAPTURE=<dir>`) and a CLI to
redact captured material; both are design-only.

- **If we want it:** add a `crates/ironclaw-host/src/fixture/`
  module with the redaction passes (bearer tokens, signing
  secrets, personal text). Expose as `ironclaw fixture redact <dir>`
  in `crates/ironclaw-host/src/bin/main.rs`. ~half-day including
  unit tests for each redaction rule.
- **If we don't:** leave the doc as-is (current edit already
  describes the manual workaround) and remove the design-doc
  bullet from `PLAN.md` M11.
- **Load-bearing question:** do we plan to capture fixtures from
  production (or staging) traffic? If no — the manual workaround
  is fine and we delete the plan entry. If yes — invest in the
  pipeline.

### Generic `iclaw approvals approve <id>` / `deny <id>`

Today only sender approvals have a write path
(`iclaw approvals approve --channel <ct> --identity <id>`). The
other approval families (Channel, InstallPackages, AddMcpServer)
exist as rows in `pending_approvals` but the operator has to
hand-CRUD them via the central DB (or re-run the workflow that
registered the row).

- **What it takes:** new `Approvals::Approve { id }` and
  `Approvals::Deny { id }` variants in
  `crates/ironclaw-iclaw/src/commands.rs`; dispatch to a new
  host-side handler that switches on the row's `action` and
  applies the appropriate per-family update. ~day with tests for
  each family.
- **Load-bearing question:** for InstallPackages / AddMcpServer
  approvals, what does "approve" mean exactly? Insert the
  packages/servers into `container_configs` and queue a rebuild?
  Just mark the row resolved and rely on the agent to re-issue?
  Decide this before writing the code or the dispatch will rot.

### Mattermost / line / generic-webhooks default ports

`line`, `mattermost`, and the generic `webhooks` channel default to
OS-assigned ports (port 0). The other HTTP-listening channels pin
8081-8087. Operators have to set a `port` explicitly to get a stable
endpoint for the reverse proxy.

- **Options:** pick a free port (8088, 8089, 8090) and set as
  `DEFAULT_PORT` in each crate's `config.rs`; OR keep dynamic and
  rely on the doc to call this out. The doc edit on 2026-05-23
  already calls it out.
- **Load-bearing question:** do we care about a stable default?
  Existing deployments rely on the dynamic behaviour. Probably
  not worth churning unless someone reports it as a footgun.

---

## Medium

### Setup wizard `channel` step for Slack / Discord

Today only Telegram has an interactive pairing wizard in
`crates/ironclaw-setup/src/steps/telegram.rs`. Slack and Discord
land via post-setup `iclaw messaging-groups create` +
`iclaw wirings create`.

- **What it takes:** for Slack: prompt for bot token, validate via
  `auth.test`, prompt for the channel id (or default to the bot's
  most-recent channel), write `SLACK_BOT_TOKEN` + the messaging
  group / wiring. For Discord: same shape with gateway token +
  guild/channel id. New step files under
  `crates/ironclaw-setup/src/steps/`; register in `step_list`.
  ~day per channel.
- **Load-bearing question:** does each platform's bot-creation
  flow lend itself to a 60-second wizard the way Telegram's does
  (BotFather → token → `/start`)? Discord's developer-portal flow
  is more involved. Probably worth offering a "you already have
  the token, paste it" branch instead of trying to guide the user
  through the platform's UI.

### Channel-level / install / MCP approval write-handlers

Companion to "Generic `iclaw approvals approve`" above — once the
CLI exists, the host-side handlers need real implementations:

- Channel approval → `messaging_groups` row creation, optionally
  with an auto-wiring step.
- InstallPackages approval → merge into
  `container_configs.packages_apt` / `packages_npm`, queue rebuild.
- AddMcpServer approval → insert into
  `container_configs.mcp_servers`, queue rebuild.

~day for all three.

### `IRONCLAW.md` per-session briefing surface

The runner already reads `<session_root>/IRONCLAW.md` and
`<groups_dir>/<id>/IRONCLAW.md` into the system prompt, but
there's no operator-facing way to create / edit either. Today
operators write the file by hand.

- **What it takes:** `iclaw groups briefing edit <id>` (TOML-style
  but plain markdown) opens `$EDITOR` on the per-group file;
  `iclaw sessions briefing edit <id>` does the same for a single
  session. Skip the upload — it's just a file-write under the
  host's data root, which the host already owns.
- **Load-bearing question:** scope. If this is "edit it like
  CLAUDE.md" — easy. If it grows into "templates / per-channel
  defaults / inheritance" — harder; cap scope explicitly.

---

## Large

### Replay-fixture coverage for the remaining 14 channels

7 channels (cli, telegram, slack, discord, matrix, github,
webhooks) have replay fixtures pinning the inbound-route → runner
→ outbound-deliver pipeline against byte-stable expected output.
The other 14 channels rely on per-adapter unit tests.

- **What it takes:** one fixture per channel, hand-authored, in
  the `fixtures/<channel>/<scenario>/` layout (see
  `docs/replay-fixtures.md`). ~2 hours per channel = ~4 days.
- **Load-bearing question:** is byte-stable diff valuable for
  every channel, or only for the ones where the adapter has rich
  pipeline interactions? RPC-only channels (signal, deltachat,
  emacs, imessage) might gain less from this than HTTP webhook
  channels.

---

## Tracked elsewhere

- Codex provider polish — pinned in CHANGELOG; the bridge works
  but the runner-side env forwarding has rough edges.
- Mutating git tools (commit/push/branch as MCP tools) — actively
  not pursued; current shape (read-only tools + `shell` for
  mutations) is fine.
- LSP bridge inside the container — design-only; deferred.
- Pre-edit / post-edit hooks (auto-formatter / type-checker piped
  back to model) — design-only; deferred.

---

## How to use this list

Pick an item, do it, delete the section from this file. The file
is intentionally a working document, not historical record —
shipped items leave the punch list rather than getting struck
through.

If you're contributing for the first time: the **small** items
are good targets. None of them require a design discussion.
