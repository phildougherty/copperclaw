---
name: save-skill
description: Durably save a reusable skill for your FUTURE sessions with save_skill — validate a SKILL.md now, and once an operator approves it lands in this group's skills and is discovered on the next spawn. Per-group only, approval-gated.
---

# save-skill

`save_skill` lets you teach yourself a repeatable procedure that
persists across sessions. When you've worked out a good, reusable way
to do something, capture it as a `SKILL.md` and save it — the **next**
session in this group discovers it automatically and can act on it.

This is a capability, **not** a registry: skills you save are visible
only to this agent group. There is no cross-group sharing.

## What happens

1. You call `save_skill` with a `name`, the full `SKILL.md` `content`,
   and a `reason`.
2. The tool validates the frontmatter **immediately**. An invalid
   `SKILL.md` (missing frontmatter, missing `name`/`description`,
   non-kebab-case name, or a frontmatter `name` that doesn't match the
   `name` you passed) is refused right now with the precise error —
   fix it and retry.
3. On valid input the request goes to an operator as an **approval**
   card. Nothing is written to disk until a human approves.
4. Once approved, the `SKILL.md` lands in this group's skills override
   directory. Your **next** session discovers and exposes it. No image
   rebuild is needed — skills are read fresh at spawn.

The change is **not** retroactive: the skill is NOT available in the
session you save it from. Save it, tell the user it's pending approval,
then move on — don't wait for it to appear this turn.

## Schema

```json
{
  "name": "my-skill",
  "content": "---\nname: my-skill\ndescription: One-line summary\n---\n# Body\n...",
  "reason": "string, non-empty — shown on the approval"
}
```

- `name` (required): kebab-case (`[a-z0-9][a-z0-9-]{0,63}`). Becomes the
  on-disk directory slug and **must equal** the frontmatter `name`.
- `content` (required): the full `SKILL.md` text, including the `---`
  YAML frontmatter. The frontmatter must carry `name` and `description`
  (and may carry an optional `allowed-tools` list, same as any skill).
- `reason` (required): why the skill is worth saving; audited on the
  approval.

## Writing the SKILL.md

A skill is a Markdown file with YAML frontmatter:

```
---
name: summarise-repo
description: How to produce a one-page summary of an unfamiliar repo.
---

# summarise-repo

1. `glob` for the README and top-level docs.
2. `grep` the entry points ...
```

Keep the body concrete and procedural — it becomes instructions your
future self reads. The frontmatter `description` powers skill relevance
scoring, so make it a real sentence, not one word.

## When to use this

- You discovered a multi-step procedure you'll want to repeat.
- The user asks you to "remember how to do X" as a repeatable recipe.

Use `agent-memory` / `memory_save` for durable **facts**; use
`save_skill` for durable **procedures** (a reusable playbook exposed as
a skill).

## Failure modes

- **Invalid frontmatter / name mismatch** — refused synchronously with
  the exact validation error. Correct the `SKILL.md` and retry.
- **No per-group skills root configured** (host without
  `COPPERCLAW_GROUPS_DIR`) — the request is refused and you're told to
  ask an operator to configure it.
- **Approval denied or never answered** — nothing is written; the skill
  simply doesn't appear next session.
