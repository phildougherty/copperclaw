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

The text becomes the new agent's persistent system prompt. Describe
the persona, the scope / constraints, and what tools and channels it
owns. Admins can rewrite later via `iclaw groups update <id>
--system-prompt '<text>'`.

## Channel routing

Set `channel` to wire the new agent to a specific channel at spawn.
Omit it for an unbound agent — you can attach one later with
`iclaw wirings create --mg <mg> --ag <ag>`, or just `send_message`
to it directly with `to: {kind: "agent", session_id: ...}`.

## Result

The tool ack carries `ToolEffectAck::Agent { session_id }`. Save the
session id — that's the address you use to reach the new agent.

## Consolidating subagent results (important)

When you spawn N subagents, their replies arrive in **your**
`messages_in` queue, not in the end user's chat. Routing is automatic:
the host's `agent_dispatch` handler walks each child's
`source_session_id` (set at spawn) and writes the reply into your
inbound. You don't have to tell children "send to agent:<your-name>"
— the runtime does it.

**Hard rules for the user-visible reply:**

1. **Wait for ALL N children to report.** Each child's reply lands as
   a chat-kind inbound row with `source_session_id` set. If you
   spawned 3 children, do NOT publish anything to the user until 3
   such rows have arrived. Count them.
2. **Do NOT publish piecemeal.** Resist the urge to post "AI report
   in, waiting for robotics..." or to fire one section as soon as the
   first child reports. From the user's view, that looks like the
   children are messaging them directly — defeating the whole point
   of consolidation.
3. **Do NOT narrate progress.** Skip "Three researchers are now
   running and will post to your Telegram" / "X minutes elapsed" /
   "here's what we have so far." A single quick acknowledgement
   ("I'll have results in a moment") is plenty; everything else
   waits for the final reply.
4. **Do NOT claim children post directly.** They don't. Saying they
   will is a lie you'll have to walk back.
5. **One consolidated `send_message`.** When all N are in, compose
   ONE reply synthesising across the children. Don't paste child
   reports verbatim; your job is the synthesis, not the relay. Quote
   sparingly when a specific fact is load-bearing.

## Notes

- New agent inherits the caller's image / MCP servers / skills /
  packages; boots cold (first-reply latency includes spawn time).
- Permissions are the new agent's own group's, not the caller's.
- Recursive spawning is allowed but expensive — each container costs
  memory and disk.

## Example

```json
{
  "name": "ReleaseNotesBot",
  "instructions": "You summarise each new release commit on the main branch and post a digest to #releases.",
  "channel": "slack:C01XYZ"
}
```
