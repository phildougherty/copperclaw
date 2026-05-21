---
name: customize
description: Guided flow for changing the agent's model, installed tools, behavior prompt, budgets, or channel wiring. Invoke when the user asks to "customize", "change something about you", "use a different model", "add a tool", or similar.
---

# customize

Walks the user through a self-modification. Most mutations to the
agent's container config are host-only (mutation tools refuse from
inside the container), so this skill alternates between tools you
*can* call (`install_packages`, `add_mcp_server`, `read_file`,
`send_message`) and printing the exact `iclaw` command the operator
must run on the host.

`iclaw` is **not** mounted inside the container — do not try to
`shell` it. Always hand the command back to the operator.
<!-- TODO(team-h): if iclaw ever gets bind-mounted into the container,
     this skill can call it directly for read-only commands. -->

## Step 1 — establish what to change

Ask the user one short question. Offer the common branches:

1. **Model / effort** — switch provider, model name, reasoning level.
2. **Tools** — install apt/npm packages or wire a new MCP server.
3. **Behavior** — adjust the system prompt (per-group CLAUDE.md).
4. **Budgets / limits** — daily token cap.
5. **Channel wiring** — out of scope; route to `/manage-channels`
   docs and stop.

Use `ask_user_question` if the channel renders buttons; plain
`send_message` otherwise.

## Step 2 — run the branch

### Model / effort

1. `shell` `cat /data/runner.json` to read the current model (the
   runner config is bind-mounted at `/data`).
2. Confirm the desired model with the user.
3. Print the exact host command and stop:

   ```
   iclaw groups config update <agent_group_id> --field model=<value>
   iclaw groups restart <agent_group_id>
   ```

   The agent's `agent_group_id` is in `/data/runner.json`.

### Tools

- **Package**: call `install_packages` directly. Explain it writes
  an approval request — the package lands after the next container
  rebuild.
- **MCP server**: call `add_mcp_server` directly. Same approval
  flow. If the server needs a binary, install the package first.
- Before either, show the user what is already configured (read
  `/data/runner.json` if it lists mcp servers, or ask the operator
  to run `iclaw groups config get <id>`).

### Behavior

1. Read the per-group CLAUDE.md if present
   (`/data/group/CLAUDE.md` is the conventional container path; if
   absent, tell the user this group has no behavior override yet).
   <!-- TODO(team-h): confirm exact in-container path for per-group
        prompt overrides once Team B finalises mount layout. -->
2. Propose the edit verbatim. Print the proposed file content and
   ask the operator to confirm.
3. On confirmation, instruct the operator:

   ```
   $EDITOR <data_dir>/groups/<folder>/CLAUDE.md
   iclaw groups restart <agent_group_id>
   ```

   Do **not** `write_file` into `/data/group/...` blindly —
   behavior changes should be auditable on the host.

### Budgets

1. Print the host command for the operator to run:

   ```
   iclaw budgets list
   ```

2. Suggest a new cap and print:

   ```
   iclaw budgets set --agent-group-id <id> --daily-tokens <N>
   ```

   `--daily-tokens 0` or `--clear` removes the cap.

## Step 3 — pickup hint

After any change, remind the operator the container must restart
to pick up the new config:

```
iclaw groups restart <agent_group_id>
```

or, for a full host bounce, `ironclaw stop && ironclaw start`.

## Triggers

- "customize yourself"
- "use a different model" / "switch to opus" / "lower effort"
- "add the github MCP server" / "install ripgrep"
- "tweak your behavior" / "you should be more concise"
- "raise/lower my budget" / "how much can you spend"

## Do NOT

- Do not call `shell` to run `iclaw` — the binary isn't in the
  container.
- Do not `write_file` to paths outside `/data` — the rest of the
  filesystem is the container image, changes won't persist.
- Do not propose a model name without confirming the provider
  supports it (Anthropic, OpenRouter, Ollama all have different
  catalogues).
- Do not request changes that would require root inside the
  container unless the operator has said they're running as root.
- Do not bundle multiple changes into one approval request — one
  `install_packages` per logical step, so the operator can approve
  or reject each cleanly.
