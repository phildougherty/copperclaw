---
name: explore
description: Open a lightweight in-process subagent to research a focused question. Use when a single answer needs reading 3+ files or running 2+ search queries. Returns one summary string; intermediate exploration never enters the parent's context.
---

# explore

`explore` is the answer to "go look at these files and tell me what's
there" without paying the cost of a full sibling agent. Internally it
opens a bounded LLM loop against the same upstream the parent uses,
with the same model, the same API key, the same base URL. The
subagent has its own conversation: it does *not* see the parent's
history. It does its work, returns a single summary string, and
disappears.

Compare with `create_agent` (heavyweight: spawns a fresh container
session, persists forever, can talk to other channels) and with
inline `read_file` calls (cheap: but the model's context fills up
with the file contents and stays full).

## When to reach for it

- A question requires reading **3 or more files** before you can
  answer with confidence.
- A question requires running **2 or more search queries** (`grep`,
  `web_search`, `web_fetch`) and reasoning across the results.
- You want a one-paragraph synthesis, not the raw file contents,
  back in the parent conversation. Burning context on intermediate
  exploration is the thing this tool exists to avoid.

## When NOT to reach for it

- Trivial single-file lookups. `read_file` is cheaper.
- Anything that mutates state. The subagent's tool allowlist
  defaults to read-only on purpose. Use the regular tools for
  writes.
- "Follow up on the conversation we just had." The subagent does
  not see the parent's history; it would have to re-derive context
  it can't reach.

## Schema

```json
{
  "task": "Find every place we call provider.query() and summarize the surrounding context.",
  "max_turns": 5,
  "max_tokens": 50000,
  "tools": ["grep", "read_file", "glob"]
}
```

- `task` (required). A self-contained instruction. **Be specific** —
  the subagent does not see your conversation history, so do not say
  "the function I mentioned earlier"; say which function.
- `max_turns` (optional, default 5, hard cap 10). LLM round-trips
  the subagent may take. Each turn = one provider call.
- `max_tokens` (optional, default 50_000, hard cap 200_000). Total
  *input* tokens budget. Output is bounded per-turn by the model's
  own `max_tokens`. The loop bails between turns if cumulative
  input crosses the cap with summary
  `"explore stopped: token budget exceeded"`.
- `tools` (optional, default `["grep", "glob", "read_file", "web_fetch"]`).
  Allowlist of tool names the subagent may invoke. Anything off the
  list comes back to the model as a synthetic refusal so it can
  recover.

## Read-only by default — keep it that way

The default allowlist is the read-only set. **Don't widen it** without
a concrete reason. Adding `shell` or `write_file` is a footgun: the
subagent might make destructive changes you can't audit because the
intermediate exploration is invisible to you. If you genuinely need
the subagent to write, write it yourself with the data the subagent
returned.

The host's `cli_scope` config can also refuse to widen the allowlist
entirely — if scope is `disabled`, only the read-only defaults pass
through regardless of what you pass in.

## Bounds the host enforces (you can't override these)

- **Hard wall-clock**: 60 seconds total. If the subagent goes
  beyond, you get whatever summary it has so far.
- **No nested explore**: a subagent that itself calls `explore` is
  refused with a validation error. The chain stops at depth 1.
- **No conversation share**: fresh context every time. The task
  string is the only input.
- **Token accounting**: input + output tokens count against the
  parent's daily token budget, same emission path as a regular
  turn.

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

- `summary` is what you usually want — feed it back to the user or
  use it as context for your next step.
- `tools_called` is an audit trail: useful for "what did the
  subagent actually do?" when the summary doesn't smell right.
- `turns_used` and `tokens_used` are the bill. If you blow the
  parent's daily token budget by spawning ten exploratory
  subagents, that's on you.

## Example

```json
{
  "task": "List every `ChannelAdapter` impl under crates/ironclaw-channels/ and for each one note the inbound + outbound method names. Return a markdown table.",
  "max_turns": 4
}
```

The subagent will `glob`/`grep` its way to a list, `read_file` a few
trait impls, and return one table. Your conversation gets the table,
not 21 channel implementations.

## Triggers

- "go look at" / "research" / "find out" / "explore"
- "summarise [several files / a directory tree]"
- "what do all the callers of X do?"
- Anything that *would* require you to read 3+ files and you'd
  prefer the synthesis back rather than the raw bytes.

## Do NOT

- Do not pass conversation context in the `task` — paraphrase what
  the subagent needs to know explicitly.
- Do not widen the tool allowlist beyond read-only without a clear
  reason; the safer pattern is "have the subagent return what it
  found, then act on it yourself".
- Do not call `explore` for a single-file lookup; `read_file`
  is faster, cheaper, and keeps the result visible to you.
- Do not call `explore` from inside another `explore` subagent —
  the host refuses it. If your task needs branching, return early
  with a partial summary and let the parent dispatch the next
  branch.
