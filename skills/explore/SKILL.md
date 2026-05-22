---
name: explore
description: Open a lightweight in-process subagent to research a focused question. Use when a single answer needs reading 3+ files or running 2+ search queries. Returns one summary string; intermediate exploration never enters the parent's context.
---

# explore

A bounded LLM loop against the same upstream the parent uses (same model,
key, base URL). The subagent has its own conversation — it does *not* see
the parent's history. It returns a single summary string and disappears.

Compare with `create_agent` (heavyweight: fresh container, persists, can
reach channels) and inline `read_file` (cheap, but fills the parent's
context with the file contents).

## When to reach for it

- A question requires reading **3+ files** before you can answer.
- A question requires **2+ search queries** (`grep`, `web_search`,
  `web_fetch`) and reasoning across the results.
- You want a paragraph of synthesis back, not raw bytes. Burning parent
  context on intermediate exploration is what this tool avoids.

## When NOT to reach for it

- Trivial single-file lookups — `read_file` is cheaper.
- Anything that mutates state. The default allowlist is read-only on
  purpose.
- "Follow up on what we just discussed" — the subagent cannot see the
  parent's history.

## Schema

```json
{
  "task": "Find every place we call provider.query() and summarize the surrounding context.",
  "max_turns": 5,
  "max_tokens": 50000,
  "tools": ["grep", "read_file", "glob"]
}
```

- `task` (required). Self-contained instruction. **Be specific** — no
  "the function I mentioned"; name it.
- `max_turns` (default 5, hard cap 10). LLM round-trips allowed.
- `max_tokens` (default 50_000, hard cap 200_000). Input-token budget.
  Loop bails between turns when crossed: `"explore stopped: token
  budget exceeded"`.
- `tools` (default `["grep", "glob", "read_file", "web_fetch"]`).
  Allowlist; off-list calls come back as synthetic refusals.

## Read-only by default — keep it that way

Don't widen the allowlist without a concrete reason. Adding `shell` or
`write_file` is a footgun: destructive changes you can't audit because
the subagent's work is invisible. If you need writes, have the subagent
return data and write it yourself.

The host's `cli_scope` can refuse to widen the allowlist entirely — if
scope is `disabled`, only the read-only defaults pass through.

## Bounds the host enforces (you can't override)

- **Hard wall-clock 60s**: past that, you get whatever summary exists.
- **No nested explore**: depth 1, refused with a validation error.
- **No context share**: fresh every time; `task` is the only input.
- **Tokens count against** the parent's daily budget, same path as a
  regular turn.

## Output shape

```json
{
  "summary": "...the assistant's final text...",
  "turns_used": 3,
  "tokens_used": 12453,
  "tools_called": [
    { "name": "grep",      "input": { "q": "provider.query(" } },
    { "name": "read_file", "input": { "path": "crates/.../run.rs" } }
  ]
}
```

- `summary` — usually what you want; feed it forward.
- `tools_called` — audit trail for when the summary smells off.
- `turns_used` / `tokens_used` — the bill.

## Triggers

- "go look at" / "research" / "find out" / "explore"
- "summarise [several files / a directory tree]"
- "what do all the callers of X do?"
- Any task that would require reading 3+ files and you'd prefer the
  synthesis back, not the raw bytes.

## Do NOT

- Don't pass parent conversation context in `task` — paraphrase what
  the subagent needs to know.
- Don't widen the allowlist beyond read-only without a clear reason.
- Don't call `explore` for a single-file lookup.
- Don't nest `explore` calls — refused at depth 1; branch from the
  parent instead.
