---
name: customize
description: Guided flow for changing the agent's model, installed tools, behavior prompt, budgets, or channel wiring. Invoke when the user asks to "customize", "change something about you", "use a different model", "add a tool", or similar.
---

# customize

Walks the user through a self-modification. Most container-config
mutations are host-only (mutation tools refuse from inside the
container), so alternate between tools you *can* call
(`install_packages`, `add_mcp_server`, `read_file`, `send_message`)
and printing the exact `cclaw` command the operator must run.

`cclaw` is **not** mounted inside the container — do not `shell` it.
Hand the command back to the operator.

## Step 1 — establish what to change

Ask one short question; offer the branches:

1. **Model / effort** — provider, model name, reasoning level.
2. **Tools** — apt/npm packages or new MCP server.
3. **Behavior** — system prompt (per-group CLAUDE.md).
4. **Budgets / limits** — daily token cap.
5. **Channel wiring** — out of scope; route to `/manage-channels`
   docs and stop.

Use `ask_user_question` if the channel renders buttons; plain
`send_message` otherwise.

## Step 2 — run the branch

### Model / effort

1. `shell cat /data/runner.json` for the current model.
2. Confirm the desired model with the user.
3. Print and stop:

   ```
   cclaw groups config update <agent_group_id> --field model=<value>
   cclaw groups restart <agent_group_id>
   ```

   `agent_group_id` is in `/data/runner.json`.

### Tools

- **Package**: call `install_packages`. Explain the package lands
  after the next container rebuild.
- **MCP server**: call `add_mcp_server`. If the server needs a
  binary, install the package first.
- Show what's already configured first: read `/data/runner.json` if
  it lists mcp servers, or ask the operator to run `cclaw groups
  config get <id>`.

### Behavior

1. Read `/data/group/CLAUDE.md` if present (the conventional
   container path); if absent, tell the user the group has no
   override yet.
2. Propose the edit verbatim; ask the operator to confirm.
3. On confirmation, instruct:

   ```
   $EDITOR <data_dir>/groups/<folder>/CLAUDE.md
   cclaw groups restart <agent_group_id>
   ```

   Do **not** `write_file` blindly into `/data/group/...` — behavior
   changes should be auditable on the host.

### Budgets

```
cclaw budgets list
cclaw budgets set --agent-group-id <id> --daily-tokens <N>
```

`--daily-tokens 0` or `--clear` removes the cap.

## Step 3 — pickup hint

After any change, the container must restart:

```
cclaw groups restart <agent_group_id>
```

Or `copperclaw stop && copperclaw start` for a full host bounce.

## Triggers

- "customize yourself"
- "use a different model" / "switch to opus" / "lower effort"
- "add the github MCP server" / "install ripgrep"
- "tweak your behavior" / "be more concise"
- "raise/lower my budget" / "how much can you spend"

## Do NOT

- `shell cclaw` — binary isn't in the container.
- `write_file` outside `/data` — rest of the FS is the image; changes
  won't persist.
- Propose a model name without confirming the provider supports it
  (Anthropic, OpenRouter, Ollama have different catalogues).
- Request changes needing root inside the container unless the
  operator said they're running as root.
- Bundle multiple changes into one approval — one `install_packages`
  per logical step so the operator can accept/reject each cleanly.
