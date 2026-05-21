---
name: debug
description: Troubleshooting flow for when the user reports a problem ("you didn't reply", "it's slow", "something seems broken"). Pulls diagnostics the agent can reach and routes the rest to the operator.
---

# debug

Investigates a user-reported issue. The agent runs *inside* the
container; the host log and `iclaw` socket are *outside*. Pull
whatever diagnostics you can from the container, then hand the
remaining commands to the operator.

## Step 1 — clarify

Ask the user, in one message: what did they observe, on which
channel, and roughly when? Don't dive into diagnostics on a vague
report — most "broken" turns out to be a missing wiring or an
expired credential, and the user's answer narrows the search.

## Step 2 — pull what you can from inside the container

- `read_file` on `/data/runner.json` to confirm session id,
  agent_group_id, model, and provider URL.
- `shell` `ls -la /data/` to see the session layout (inbound DB,
  outbound DB, attachments). If `outbound.db` exists, the agent has
  been sending — failures are downstream.
- `shell` to scan any session-scoped log files
  (`/data/*.log` if present).
  <!-- TODO(team-h): if Team B starts routing host-side logs into
       the session dir, document the exact path here so we can
       `read_file` directly. -->

Host-side data is **not** reachable from `read_file`. The host log
lives at `<data_dir>/logs/ironclaw.out.log` (and `.err.log`) and
the `iclaw` socket is the only way into the central DB. Both are
operator-side.

## Step 3 — route the rest to the operator

Print these commands and ask the operator to paste the results
back:

```
iclaw health                                   # structured health probe
iclaw status                                   # wiring digest
iclaw audit list --since 1h                    # recent mutations
iclaw dropped-messages list --since 1h         # delivery failures
iclaw sessions get <session_id>                # this session's state
```

(There is no `iclaw doctor` — `iclaw health` is the equivalent.)

The host log is at `<data_dir>/logs/ironclaw.out.log` and
`<data_dir>/logs/ironclaw.err.log`. Ask the operator to grep for
`ERROR` or `WARN` in the last 10 minutes:

```
tail -n 500 <data_dir>/logs/ironclaw.err.log | grep -iE 'error|warn'
```

## Step 4 — synthesize

Once you have the diagnostics, write a single message that names:

- **What's broken** (one sentence).
- **Likely cause** (one sentence).
- **What to do** (concrete next command, or "rerun setup", or
  "rotate the credential at `<path>`").

Common patterns:

- Delivery rows in `dropped-messages` with `Auth` failures → channel
  credential expired. Operator runs `ironclaw-setup` or rotates the
  token via `iclaw destinations update ...`.
- `audit list` shows recent failed `groups.config.update` → policy
  rejected the mutation; check approvals: `iclaw approvals list`.
- No outbound rows but inbound has new entries → runner stalled.
  `iclaw groups restart <id>`.
- `health` reports degraded with stuck sessions → ironclaw bounce:
  `ironclaw stop && ironclaw start`.

## Step 5 — fall through

If nothing in the diagnostics explains it, ask 1-2 follow-up
questions: exact phrasing of the request that didn't get a reply,
the channel's own UI state (Slack "sending..." spinner, Telegram
checkmarks), whether other agents on the same install respond.

## Triggers

- "you didn't reply"
- "you're slow" / "nothing happens"
- "I think you're broken" / "this isn't working"
- "messages aren't going through"
- "did you get my last message"

## Do NOT

- Do not call `shell` to run `iclaw` — `iclaw` is not on the
  container's PATH and the socket isn't bind-mounted. Always route
  iclaw commands to the operator.
- Do not retry sending the message that "didn't go through" until
  diagnostics rule out a duplicate-delivery race — the host's
  delivery loop already retries up to 3 times.
- Do not change configuration (model, MCP, budgets) as part of
  triage — that's `/customize`. Get to root cause first.
- Do not surface raw error messages without translation; explain
  what they mean for the user.
