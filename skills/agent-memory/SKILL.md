---
name: agent-memory
description: Read and write persistent per-agent-group memory at `/data/memory/`. Survives across sessions of the same group; use it to remember who the user is, how they prefer to be talked to, and decisions they made earlier.
---

# agent-memory

A persistent, file-based memory built up across conversations. Lives
under `/data/memory/` inside your container, bind-mounted per agent
group — every session of this group reads and writes the same files.
Use the existing `read_file` and `write_file` tools; no special tool
required.

If `/data/memory/` does not exist when you reach for it, the operator
has not configured `groups_dir`. Behave as a stateless agent and don't
fabricate continuity.

## Types of memory

One file per entry, categorised by frontmatter `type`:

- **`user`** — who the user is. Role, expertise, preferences for how
  they want to interact. E.g. "Senior backend engineer, new to React —
  frame frontend explanations in terms of backend analogues."
- **`feedback`** — guidance about how to work. Corrections ("don't
  summarise the diff") and validated choices ("the single PR was right
  here"). Record *why* so you can judge edge cases later.
- **`project`** — ongoing initiatives, deadlines, decisions. Convert
  relative dates to absolute when writing ("Thursday" → "2026-03-05").
- **`reference`** — pointers to external systems ("pipeline bugs
  tracked in Linear project INGEST").

## What NOT to save

- Anything derivable from the live filesystem (project structure, code
  patterns, file paths, git history — read the source).
- Ephemeral state ("currently working on X") — that's [[todo-tracker]],
  scoped to one session.
- Conversation transcripts — the runner already persists chat history.

These hold even when the user says "remember this." If what they're
asking you to save is implicit in the code or in git history, ask what
was *surprising* about it — the surprising part is what's worth
keeping.

## File layout

```
/data/memory/
  MEMORY.md
  user_role.md
  feedback_terse_replies.md
  project_auth_rewrite.md
  reference_linear_ingest.md
```

`MEMORY.md` is a one-line-per-entry index, read first to see what's
available:

```markdown
- [User role](user_role.md) — senior backend, new to React
- [Terse replies](feedback_terse_replies.md) — no trailing summaries
- [Auth rewrite](project_auth_rewrite.md) — compliance-driven scope
```

Each entry file has YAML frontmatter and a body:

```markdown
---
name: feedback-terse-replies
description: User wants tight replies, no trailing "I did X, Y, Z" summary
metadata:
  type: feedback
---

User asked not to summarise work at the end of a reply.

**Why:** "I can read the diff" — summary is duplicate effort.

**How to apply:** Stop after the substantive content. No closing
paragraph that restates what was done.
```

Link related memories with `[[name]]` (the frontmatter `name`). Liberal
linking is fine; an unresolved `[[name]]` marks something worth writing
later.

## When to read

- The user references something that sounds like a continuation ("like
  we discussed", "the usual way").
- About to make a stylistic or scope choice the user has weighed in on
  before — read `feedback_*` first.
- About to ask the user basic facts about themselves you might already
  know — check `user_*` first.

## When to write

- User explicitly asks you to remember something.
- User corrects how you're working in a way that applies beyond this
  session.
- User confirms a non-obvious choice ("yes exactly, keep doing that")
  — write it so you don't second-guess next time.

Don't write a memory just because something happened. Memory is for
facts useful next week.

## Updating

If a recalled memory conflicts with what you observe now, trust what
you observe and update or delete the file. Stale memory is worse than
no memory.
