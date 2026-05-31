---
name: discovering-tools
description: Inventory the MCP tools available to this agent, introspect their schemas, and identify which servers contributed them.
---

# discovering-tools

The agent's tool list is the union of:

- The built-in tools served by the in-process MCP server
  (`copperclaw-mcp`). The registry is built in
  `crates/copperclaw-mcp/src/tools/mod.rs::build_tool_set` — that's
  the canonical inventory. Always-on, no admin step.
- External MCP servers configured via `container_configs.mcp_servers`
  (which an admin or the agent itself registered through
  `add_mcp_server`).

This skill explains how to enumerate the live set and read each
tool's JSON schema.

## Built-in tools (categories)

The in-process MCP server exposes around three dozen built-in tools,
grouped roughly:

- **Messaging core** — `send_message`, `send_file`, `edit_message`,
  `add_reaction`.
- **Interactive UI** — `ask_user_question`, `send_card`.
- **Agent lifecycle** — `create_agent`.
- **Self-modification (gated)** — `install_packages`, `add_mcp_server`.
- **Scheduling** — `schedule_task`, `list_tasks`, `cancel_task`,
  `pause_task`, `resume_task`, `update_task`.
- **Computer use** — `shell`, `read_file`, `write_file`, `edit_file`,
  `web_fetch`, `artifact_path`.
- **Code navigation** — `grep`, `glob`, `git_status`, `git_diff`,
  `git_log`, `git_blame`.
- **Research** — `web_search`, `explore` (read-only subagent).
- **Skill catalogue** — `load_skill`.
- **Planning scratchpad** — `todo_add`, `todo_list`, `todo_update`,
  `todo_delete`.
- **Session control** — `compact_now`, `clear_history`.

For the authoritative list at any moment, call `tools/list` (the
runner proxies it). The names above are stable; the count drifts as
new tools land — don't trust a hard number quoted in a doc.

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
`crates/copperclaw-mcp/src/tools/*.rs`. Standard shape:
`additionalProperties: false`, an explicit `required` list, and
`minLength: 1` on string fields meaning "non-empty". External tool
schemas come from the upstream MCP server; treat as advisory.

## Suppressed names

Some upstream Claude-SDK / MCP names are deliberately not exposed
(use the local analogue instead):

- `CronCreate` / `CronDelete` / `CronList` / `ScheduleWakeup` →
  use `schedule_task` & friends.
- upstream `AskUserQuestion` → use the local `ask_user_question`.
- `EnterPlanMode` / `ExitPlanMode` / `EnterWorktree` / `ExitWorktree`
  → not surfaced.

## Practical pattern

Begin every fresh session by trusting `tools/list` (the provider's
SDK calls it automatically). Don't hard-code names from training
data — the actual set is what the runner reports.
