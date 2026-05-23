# Agent-to-agent routing — root-cause plan

## Status

- **Immediate workaround shipped:** `agent_to_agent.rs::CreateAgentHandler`
  now copies the parent session's `messaging_group_id` and `thread_id`
  onto every spawned child session. A child that calls `send_message`
  without an explicit `to:` now replies into the same Telegram chat
  (or Slack thread, etc.) the parent was reading. Test:
  `child_session_inherits_parent_messaging_group`.
- **Root cause still open:** the addressing model for multi-agent
  hierarchies is implicit. The workaround papers over symptoms; this
  doc describes what a real model should look like.

## Why "inherit parent's wiring" isn't the right answer long-term

The current behaviour treats every child as a "deputy" speaking in the
parent's voice into the parent's chat. That's right for some workloads
(quick research agent that needs to drop one summary line back to the
user) and wrong for others:

1. **Parent doesn't want children spamming the user channel.** When a
   parent spawns five scouts and each calls `send_message("status: …")`,
   the user gets five messages from "the agent" with no provenance.
   Today the only way to suppress this is for the child to call
   `send_message(to: "agent:parent-name")` — but most agent prompts
   never learn this convention.
2. **Children may legitimately want to ask the user something** (an
   ASK skill that needs a clarification) — but they should ask
   *through* the parent, not appear as an independent voice in the
   user's chat.
3. **Grandchildren are ambiguous.** With subagent depth raised to 3,
   a grandchild's "default" recipient could be the parent, the
   grandparent, or the user. The current `inherit messaging_group_id`
   silently picks "the user."
4. **Channel-bound wiring is sticky.** If the parent later changes
   channels (e.g. via `iclaw groups change-channel`) the child
   sessions still point at the old MG until they're recreated.

## What the proper model should provide

Three addressing primitives the agent prompt can rely on:

| Destination | When to use | Wire path |
|---|---|---|
| `to: "user"` | Replying to the human in the conversation the agent is in | resolve current session's MG + thread |
| `to: "agent:parent"` | Reporting back to the agent that spawned you | walk source_session_id; deliver into parent's `inbound.db` |
| `to: "agent:<name>"` | Sending to a named sibling | existing destinations table lookup |

The current model only natively supports the last; "user" and "parent"
have to be encoded as the same thing (parent's MG inheritance) or
explicit slug lookups (`agent:parent-name` which requires the child
to know the parent's name — and prompt authors routinely forget).

## Proposed plan

### Phase 1 — Persist the parent chain (migration)

Add `sessions.source_session_id TEXT REFERENCES sessions(id)` via a new
migration. Populate on every `create_agent` from `parent_for_check`.
This unlocks the `to: "agent:parent"` semantics and is also the right
shape for depth-tracking (we already have `agent_groups.subagent_depth`
but that's a flat number; chain walks are more useful for routing).

Code touch points:
- `crates/ironclaw-db/migrations/013_sessions_source_session.sql`
- `crates/ironclaw-db/src/tables/sessions.rs` — `Session` struct,
  `CreateSession`, all SELECTs/INSERTs.
- `crates/ironclaw-modules/src/agent_to_agent.rs` — set
  `source_session_id` when creating the child session (alongside the
  messaging-group inheritance we already do).

### Phase 2 — Replace MG inheritance with explicit "user" / "parent" resolution

Currently `send_message(to: None)` (no `to:` field) defaults to the
session's `messaging_group_id`. After phase 1 we can split:

- `send_message(to: "user", text: …)` — explicit "to the human"
  routing. The runner resolves via the *root* session's MG (walk
  source_session_id chain to the top). This is the same recipient
  the original user message came from, regardless of how deep the
  spawn went.
- `send_message(to: "agent:parent", text: …)` — write into
  `<parent_session>/inbound.db` as a system message so the parent's
  next turn sees the child's report.
- `send_message(to: None, text: …)` — *fall back to "agent:parent"*
  when the session has a `source_session_id`, else "user". This
  inverts today's default — children now report up by default,
  never to the user — and makes the surprising case (cross-talking
  to the user) explicit.

### Phase 3 — Skill prompt updates

The `send-message` skill currently says "use `to: …` to override the
default destination." Update it to document the three primitives and
make "agent:parent" the recommended default for child agents (they
should aggregate findings and let the parent decide what to surface
to the user). Coding agents in deep tool loops get the parent-default
for free; messaging agents at depth 0 still talk to the user.

### Phase 4 — Surface the chain in observability

`iclaw audit list` and `iclaw health` should expose the parent →
child relationships so operators can see "Telegram session → agent
spawned scout A → scout A spawned ScoutA.1" without joining tables
by hand. Small CLI work; pure SELECT additions.

## Why not do all of this now

- Phase 1 needs a migration; we just shipped 012 (`coding_enabled`).
  Stacking migrations is fine but each one should land with its
  consumer fully wired, not as plumbing-only.
- Phase 2 changes the runner's send_message default behaviour. Needs
  fixture coverage in `crates/ironclaw-host/tests/replay/` so we
  don't regress existing single-agent flows.
- Phase 3 touches every operator's existing agent prompts.

Sequencing this properly is one focused contribution per phase, not a
weekend cram. The workaround we shipped today is what unblocks the
live Telegram use case while the proper model is built.

## Verification once shipped

End-to-end fixture: spawn parent → child → grandchild via `create_agent`.
Have the grandchild call `send_message(to: "user", text: …)` and assert
the message lands in the parent-of-parent's `outbound.db` for the root
MG. Have the grandchild call `send_message(to: None, …)` and assert it
lands in the *parent's* `inbound.db` (not the user's). Have the parent
then summarise and call `send_message(to: "user", …)`. The replay-fixture
harness can pin all of this byte-stably.
