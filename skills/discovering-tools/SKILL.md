---
name: discovering-tools
description: Inventory the MCP tools available to this agent, introspect their schemas, and identify which servers contributed them.
---

# discovering-tools

The agent's tool list is the union of:

- The 15 built-in tools served by the in-process MCP server
  (`ironclaw-mcp`).
- External MCP servers configured via `container_configs.mcp_servers`
  (which an admin or the agent itself registered through
  `add_mcp_server`).

This skill explains how to enumerate the live set and read each
tool's JSON schema.

## Built-in tools

The in-process MCP server exposes these 15 tools (see PLAN.md § 7 for
the canonical inventory):

| Tool | Module | One-line purpose |
|---|---|---|
| `send_message`      | core         | reply or cross-post text |
| `send_file`         | core         | reply with a file attachment |
| `edit_message`      | core         | replace the body of a sent message |
| `add_reaction`      | core         | react to a sent message |
| `ask_user_question` | interactive  | constrained multiple-choice |
| `send_card`         | interactive  | platform-specific structured UI |
| `create_agent`      | agents       | spin up a sibling agent |
| `install_packages`  | self-mod     | request apt/npm installs |
| `add_mcp_server`    | self-mod     | request a new MCP server |
| `schedule_task`     | scheduling   | enqueue a one-shot or recurring prompt |
| `list_tasks`        | scheduling   | inventory scheduled tasks |
| `cancel_task`       | scheduling   | remove a task |
| `pause_task`        | scheduling   | stop firing |
| `resume_task`       | scheduling   | re-arm a paused task |
| `update_task`       | scheduling   | edit a task in place |

These are always present. They never require an admin to enable.

## External tools

Any MCP server in `container_configs.mcp_servers` contributes its
tool list to the agent's tool surface. The runner namespaces them as
`mcp__<server_name>__<tool_name>` so they cannot shadow built-ins.

For example, an admin who registered a `github` MCP server might
expose `mcp__github__list_issues`, `mcp__github__get_pr`, etc.

## Listing what is actually live

The MCP protocol's `tools/list` is the source of truth. Agents that
need to introspect at runtime ask the runner (which proxies the
call) for the current list. The response includes:

- `name` (string).
- `description` (string).
- `inputSchema` (JSON Schema describing the arguments).

If you are inspecting from outside the container (e.g. building a
prompt or generating documentation), read
`container_configs.mcp_servers` to see what is configured, then
spawn a test runner to see the live `tools/list` output.

## Reading a tool's schema

Every built-in tool's schema is declared verbatim in
`crates/ironclaw-mcp/src/tools/*.rs`. The shape always looks like:

```json
{
  "type": "object",
  "additionalProperties": false,
  "required": [ ... ],
  "properties": {
    "field": { "type": "...", "minLength": 1 }
  }
}
```

You can rely on:

- `required` listing the mandatory fields.
- `additionalProperties: false` — passing extra fields is a
  validation error.
- `minLength: 1` on string fields meaning "non-empty".

For external tools, the schema lives in the upstream MCP server's
code; treat it as advisory and handle validation errors at runtime.

## Disallowed built-ins (do not expect them)

A few tool names exist in upstream MCP definitions but are
**deliberately not** exposed by ironclaw's runner — the host owns
the underlying functionality:

- `CronCreate`, `CronDelete`, `CronList` — use `schedule_task`,
  `list_tasks`, `cancel_task` instead.
- `ScheduleWakeup` — same path, via `schedule_task`.
- `AskUserQuestion` (the upstream variant) — use the local
  `ask_user_question`.
- `EnterPlanMode`, `ExitPlanMode`, `EnterWorktree`, `ExitWorktree`
  — these are upstream Claude SDK concepts; the host does not
  surface them.

If you see one of these listed in `tools/list`, something is
misconfigured.

## Practical pattern

Begin every fresh session by calling `tools/list` (the provider's
SDK does this automatically) and trusting that list. Do not
hard-code tool names from training data — the actual set is the
one the runner reports.
