# cli/budget-exhausted

Exercises the **daily-token-cap gate** in `ContainerManager::maybe_spawn`.

`central.sql` seeds:

- A standard `agent_groups` + `messaging_groups` + wiring (identical to
  the happy-path fixtures).
- A `group_budgets` row with `daily_token_cap = 100`.
- A single `agent_turns` row for the same group with 200 tokens
  already spent today.

Two inbound events arrive back-to-back. For each:

1. The router writes a `messages_in` row.
2. The harness drives `ContainerManager::tick()`, which runs
   `is_over_budget` (200 >= 100), refuses to spawn, and writes a
   "budget exhausted" Chat reply to `messages_out`.
3. The delivery loop fans the reply back through the `cli` adapter.

The second inbound exercises the per-agent-group **dedup window** —
the budget-exhausted reply is posted only once per hour, so only the
first inbound produces a reply.

Assertions:

- `messages-in.jsonl` — two `chat`-kind rows (router did write inbound).
- `messages-out.jsonl` — one `chat`-kind row carrying the budget
  exhausted notice (deduped).
- `delivered.jsonl` — one delivery on `cli/stdin`.
