---
name: create-agent
description: Spawn a sibling agent with create_agent — name, instructions, optional channel binding, and how messages route back.
---

# create-agent

`create_agent` asks the host to spin up a fresh sibling agent next to
the current one. The new agent has its own session, its own
`messages_in` / `messages_out` databases, and its own container. It
shares the calling agent's group config (skills, MCP servers, packages)
unless an admin moves it to a different group later.

## Schema

```json
{
  "name": "Greeter",
  "instructions": "You greet new users in the welcome channel.",
  "channel": "telegram:chat-9"
}
```

- `name` (required, non-blank). Becomes the agent group's display name
  in the central DB. Used in audit logs, in `iclaw groups list`, and as
  the sender display name when the new agent emits messages.
- `instructions` (required, non-blank). The system prompt of the new
  agent. Write this the way you would write a `claude.system_prompt`:
  describe the persona, scope, and the channels it owns.
- `channel` (optional). A fully-qualified channel id the new agent
  should be wired to immediately. If absent, the new agent boots
  unbound and waits for an admin to attach it via `iclaw wirings create`.

## What "instructions" means

The text becomes the new agent's persistent system prompt. Be precise
about scope:

- Describe the persona (who the agent is impersonating).
- Describe constraints (what topics are off-limits, what privacy
  rules apply, who it answers to).
- Describe its tools (which MCP servers, which skills, which channels
  it may post to).

You can rewrite instructions later by editing `agent_groups.system_prompt`
via `iclaw groups update <id> --system-prompt '<text>'` (admin only).

## Channel routing

If you set `channel`, the host creates a wiring row pointing the
channel at the new agent so that any inbound traffic on that channel
goes to the new agent (subject to the wiring's engage / pattern).

If you omit `channel`, you must arrange a separate route — either via
`iclaw wirings create --mg <messaging-group-id> --ag <new-agent-group-id>`,
or by sending the new agent messages via `send_message(to=Agent{...})`.

## Result

The tool ack carries a `ToolEffectAck::Agent { session_id }` payload.
Save the session id — it is the address you use to reach the new
agent later:

```json
{ "to": { "kind": "agent", "session_id": "<that-session-id>" },
  "text": "Welcome, here's your first job." }
```

## Consolidating subagent results (important)

When you spawn subagents to do parallel work (research fan-out,
multi-step build, etc.), the children's replies arrive in **your**
`messages_in` queue as new chat messages, not in the end user's chat.
Routing is automatic: the host's `agent_dispatch` handler walks each
child's `source_session_id` (set when the child was created) and writes
the reply directly into your inbound. You don't have to tell the
children "send to agent:<your-name>" — the runtime does it.

Your job is to:

1. **Wait** for the subagent results to arrive (subsequent turns).
2. **Consolidate** the per-child reports into a single coherent
   answer.
3. **Send the consolidated answer** to the end user with a normal
   `send_message` (no `to:`).

Do NOT forward each subagent's raw output directly to the user as
separate messages — that recreates the disjointed-voices UX the
consolidation step is meant to prevent.

## Notes

- The new agent shares the calling agent's container image, MCP
  servers, skills, and apt/npm packages.
- The new container boots cold; first response latency includes
  the runtime's spawn time.
- The new agent is **not** subject to the caller's permissions; it
  runs with whatever policies its assigned group has.
- Creating sibling agents recursively is allowed but discouraged —
  every new container costs memory and disk.

## Example

```json
{
  "name": "ReleaseNotesBot",
  "instructions": "You summarise each new release commit on the main branch and post a digest to #releases.",
  "channel": "slack:C01XYZ"
}
```
