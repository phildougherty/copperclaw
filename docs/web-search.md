# Web search

The agent's `web_search` tool returns `{title, url, snippet}` entries
for a query. It is a thin shim over one of four supported providers;
which one runs depends entirely on which API key the operator has
configured. This doc covers the operator-side setup. The agent-side
usage is documented in [`skills/web-search/SKILL.md`](../skills/web-search/SKILL.md).

## Quick start

Pick a provider, set its env var in the host's `.env`, restart the
host:

```
# Pick one (or several — see selection below):
TAVILY_API_KEY=tvly-...
EXA_API_KEY=...
BRAVE_SEARCH_API_KEY=BSA...
SERPAPI_API_KEY=...
```

That's it. The host's container manager auto-forwards these into
every spawned session, the runner picks them up, and the agent can
call `web_search` on the next message.

Verify by asking the agent a question that requires a fresh URL:

```
> Use web_search to find the canonical Tokio website and report the
> exact URL.
agent> https://tokio.rs (via Tavily)
```

## Supported providers

| Provider | Env var | Strengths | Pricing model |
|---|---|---|---|
| [Tavily](https://docs.tavily.com/) | `TAVILY_API_KEY` | Agent-tuned; clean, short snippets; default | Per-call, free tier available |
| [Exa](https://docs.exa.ai/) | `EXA_API_KEY` | Neural / semantic search; "find conceptually similar pages" | Per-call, generous free tier |
| [Brave](https://brave.com/search/api/) | `BRAVE_SEARCH_API_KEY` | Independent web index; keyword-friendly; privacy-focused | Free tier (2k/mo) + paid |
| [SerpAPI](https://serpapi.com/) | `SERPAPI_API_KEY` | Wraps Google / Bing / DuckDuckGo / etc. | Per-call, paid |

The choice is yours; Tavily is the recommended default because it's
purpose-built for agent workloads and its snippets need the least
post-processing.

## Provider selection at call time

Resolution order on every `web_search` invocation, first match wins:

1. The explicit `provider` argument in the tool call. The agent can
   override per-call when it has a reason — e.g. neural search for a
   conceptual query.
2. `IRONCLAW_WEB_SEARCH_PROVIDER` — operator-set default. Useful
   when multiple keys are configured but the operator wants to pin
   a specific backend.
3. Auto-detect: first non-empty key in the order `tavily, exa,
   brave, serpapi`. Tavily wins by default.

If the agent asks for a provider whose key is not configured, the
tool returns a validation error pointing to the env var the operator
needs to set. The call does not silently fall back to a different
provider — secure-by-default beats surprise behaviour.

## Configuring multiple providers

There's no harm in setting more than one key. The agent can then
pick `provider: exa` for semantic queries and let the default
(Tavily) handle the rest. Quotas are per-provider, so spreading load
across providers can also be a cost optimisation.

```
# .env — Tavily as default, Exa available for semantic queries
TAVILY_API_KEY=tvly-...
EXA_API_KEY=...
IRONCLAW_WEB_SEARCH_PROVIDER=tavily
```

## Forwarding into containers

The host reads these vars from its own environment (or its `.env`)
at boot and forwards them into every session container at spawn
time via `ContainerSpec`. The forwarding list is intentionally
small — see `collect_forward_env` in
[`crates/ironclaw-host/src/boot.rs`](../crates/ironclaw-host/src/boot.rs).

What this means in practice:

- **The keys live in `.env`, never in `container_configs`.** Rotating
  a key is a host restart, not a database write.
- **They are not visible in `iclaw audit list`.** The audit log
  records the `web_search` tool calls themselves, but not the env
  forwarded into the container.
- **They are visible to anything running in the container.** The
  agent can `shell { "command": "env" }` and see them in plain text.
  Treat the container as the trust boundary; the search providers'
  API keys are essentially equivalent in privilege to the
  `ANTHROPIC_API_KEY` you already trust the container with.

## Interaction with the egress allow-list

If you've set `container_configs.egress_allow` (see
[`docs/container-config.md`](container-config.md)), you must
include the relevant provider host:

- Tavily: `api.tavily.com:443`
- Exa: `api.exa.ai:443`
- Brave: `api.search.brave.com:443`
- SerpAPI: `serpapi.com:443`

Without the entry, `web_search` will fail with a connection error
even though the API key is correct.

## When to skip `web_search` entirely

If everything your agent needs to know lives in a system you can
front with an MCP server (Linear, GitHub, Notion, Postgres, an
internal docs index), wiring that up via
[`iclaw mcp add <preset>`](container-config.md#image-rebuild-on-container_configs-change)
is strictly better than searching the public web for the same data.
The result is authenticated, structured, and not subject to a
third-party search provider's index freshness.

`web_search` is the right answer when the question genuinely
requires the open web — current events, public docs, third-party
project pages, anything not behind an integration you already own.

## Output

Every provider's response is normalised to the same shape. From the
operator's perspective, this means the agent's downstream behaviour
is provider-agnostic — switching from Tavily to Brave by flipping
`IRONCLAW_WEB_SEARCH_PROVIDER` doesn't require any changes in the
skill docs or in the agent's prompting. See
[`skills/web-search/SKILL.md`](../skills/web-search/SKILL.md) for
the exact shape.

## Costs and quotas

Per-provider defaults (subject to provider's own quota updates):

- **Tavily**: free tier ~1k searches/month; paid plans available.
- **Exa**: generous free tier; pay-as-you-go beyond.
- **Brave**: 2k/month free; paid plans for higher volume.
- **SerpAPI**: paid-only, ~$50/mo for the entry plan.

Pair with the `iclaw budgets` token cap (see PLAN.md M13 cost/
safety) to bound the number of LLM calls per group per day; one
`web_search` call costs an LLM turn plus the provider's
per-search fee. The host does not currently track per-provider
search counts as a metric (Prometheus surface only covers LLM
turns and container lifecycle today). If this matters for your
deployment, file feedback.
