---
name: web-search
description: Search the open web with the web_search tool — auto-routes to Tavily, Exa, Brave, or SerpAPI depending on which API key the operator has wired up, with a normalised result shape.
---

# web-search

`web_search` finds URLs and snippets for a query. Pair it with
`web_fetch` to read the page content: search returns a list of
`{title, url, snippet}` entries, then fetch the URL of whichever one
looks most relevant.

The tool routes to one of four supported providers based on
operator configuration. You don't pick — the host auto-detects from
configured API keys. You **can** force a specific provider per call
when you have a reason (e.g. you want neural semantic search even
though Tavily is the default).

## Schema

```json
{
  "query": "string, non-empty",
  "max_results": "integer (optional, 1-25, default 10)",
  "provider": "string (optional - tavily | exa | brave | serpapi)",
  "search_type": "string (optional - provider-specific hint)"
}
```

- `query` (required). The search string. Phrasing matters less for
  neural providers (Exa) and more for keyword providers (Brave).
- `max_results` (optional). Capped at 25 regardless of the provider's
  own ceiling. Default 10.
- `provider` (optional). One of `tavily`, `exa`, `brave`, `serpapi`.
  Validation error if the chosen provider's API key is not set in
  the container env.
- `search_type` (optional). Provider-specific hint, ignored when the
  provider doesn't recognise it:
  - **Tavily**: `basic` / `advanced` (depth) or `news` / `general` /
    `finance` (topic).
  - **Exa**: `auto` (default) / `neural` / `keyword`.
  - **Brave**: ignored.
  - **SerpAPI**: the search engine name (`google` default; `bing`,
    `duckduckgo`, etc. supported).

## Output

Every provider's response is normalised to the same shape:

```json
{
  "query": "rust async runtimes",
  "provider": "tavily",
  "elapsed_ms": 412,
  "results": [
    {
      "title": "Tokio — async runtime",
      "url": "https://tokio.rs",
      "snippet": "Tokio is an asynchronous runtime for the Rust programming language...",
      "published": "2025-01-15T00:00:00Z",
      "score": 0.92
    }
  ]
}
```

Snippets are capped at 4 KiB per result with a trailing `…` so a
verbose provider can't blow your context window. `score` is
provider-specific and omitted when the backend doesn't surface one
(Brave omits scores entirely; SerpAPI derives a `1/position` score
that's only useful for relative ranking).

## Provider quick reference

| Provider | Best for | API key env var |
|---|---|---|
| **Tavily** | Default — agent-tuned snippets | `TAVILY_API_KEY` |
| **Exa** | Semantic / neural lookups, "find conceptually similar" | `EXA_API_KEY` |
| **Brave** | Keyword search on an independent index | `BRAVE_SEARCH_API_KEY` |
| **SerpAPI** | Google/Bing/DDG wrapper; broadest coverage | `SERPAPI_API_KEY` |

If multiple keys are present and the operator hasn't set
`IRONCLAW_WEB_SEARCH_PROVIDER`, the default is the first available
in the order `tavily, exa, brave, serpapi`. Tavily wins by default
because it returns the cleanest agent-facing snippets out of the
box.

## Examples

Default provider, default count:

```json
{ "query": "what is the half-life of caffeine in adults" }
```

Force a semantic provider for a conceptual query:

```json
{
  "query": "frameworks similar to React but with stronger type safety",
  "provider": "exa",
  "search_type": "neural"
}
```

Recent news (Tavily topic hint):

```json
{
  "query": "claude 4 release notes",
  "provider": "tavily",
  "search_type": "news"
}
```

Engine-specific via SerpAPI:

```json
{ "query": "site:reddit.com vim plugins 2025", "provider": "serpapi" }
```

## Pair with `web_fetch`

The intended workflow is two calls per question:

1. `web_search { "query": "..." }` — get a candidate URL.
2. `web_fetch { "url": "..." }` — read the page.

Tavily and Exa snippets are often enough to answer simple questions
without the second call. For "what does this page actually say?",
fetch the URL.

## When to skip search

- **You already have the URL.** Just `web_fetch`.
- **The operator has an MCP server with the right access wired up.**
  Calling a Linear / GitHub / Notion MCP server is almost always
  better than searching the public web for the same data —
  authenticated, structured, more reliable.
- **The user asked a math / reasoning / explanation question.**
  Reaching for search when you already know the answer wastes the
  user's time and the operator's budget.

## Failure modes

- **No API key configured.** Returns a validation error naming the
  four env vars (`TAVILY_API_KEY`, `EXA_API_KEY`,
  `BRAVE_SEARCH_API_KEY`, `SERPAPI_API_KEY`). Surface to the user
  via `send_message` so the operator knows what to set.
- **Quota / 429 / 5xx from the provider.** Returns an internal
  error containing the HTTP status and the provider's error
  message. Consider falling back to a different provider by passing
  `provider: <other>` on retry — if multiple keys are configured.
- **Egress allow-list blocks the provider host.** The same connection
  error you'd see from `web_fetch`. Ask the operator to add the
  provider's API host to `container_configs.egress_allow`.
