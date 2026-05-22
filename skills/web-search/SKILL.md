---
name: web-search
description: Search the open web with the web_search tool — auto-routes to Tavily, Exa, Brave, or SerpAPI depending on which API key the operator has wired up, with a normalised result shape.
---

# web-search

`web_search` finds URLs and snippets. Pair with `web_fetch` to read the
page: search returns `{title, url, snippet}` entries; fetch whichever
URL looks most relevant.

The host auto-routes to one of four providers based on configured API
keys. You can force a provider per call when you want (e.g. neural
semantic search even though Tavily is the default).

## Schema

```json
{
  "query": "string, non-empty",
  "max_results": "integer (optional, 1-25, default 10)",
  "provider": "string (optional - tavily | exa | brave | serpapi)",
  "search_type": "string (optional - provider-specific hint)"
}
```

- `query` (required). Phrasing matters less for neural providers
  (Exa), more for keyword providers (Brave).
- `max_results` (optional). Capped at 25; default 10.
- `provider` (optional). Validation error when the chosen provider's
  API key isn't set in the container env.
- `search_type` (optional). Provider-specific hint, ignored when not
  recognised:
  - **Tavily**: `basic` / `advanced` (depth) or `news` / `general` /
    `finance` (topic).
  - **Exa**: `auto` (default) / `neural` / `keyword`.
  - **Brave**: ignored.
  - **SerpAPI**: engine name (`google` default; `bing`, `duckduckgo`,
    etc.).

## Output

Every provider's response is normalised:

```json
{
  "query": "rust async runtimes",
  "provider": "tavily",
  "elapsed_ms": 412,
  "results": [
    {
      "title": "Tokio — async runtime",
      "url": "https://tokio.rs",
      "snippet": "Tokio is an asynchronous runtime…",
      "published": "2025-01-15T00:00:00Z",
      "score": 0.92
    }
  ]
}
```

Snippets are capped at 4 KiB per result with a trailing `…` so a
verbose provider can't blow your context window. `score` is
provider-specific and omitted when the backend doesn't expose one
(Brave omits entirely; SerpAPI derives `1/position`).

## Provider quick reference

| Provider | Best for | API key env var |
|---|---|---|
| **Tavily** | Default — agent-tuned snippets | `TAVILY_API_KEY` |
| **Exa** | Semantic / neural lookups | `EXA_API_KEY` |
| **Brave** | Keyword search, independent index | `BRAVE_SEARCH_API_KEY` |
| **SerpAPI** | Google/Bing/DDG wrapper | `SERPAPI_API_KEY` |

When multiple keys are present and `IRONCLAW_WEB_SEARCH_PROVIDER` is
unset, default order is `tavily, exa, brave, serpapi`.

## Examples

```json
{ "query": "what is the half-life of caffeine in adults" }
```

```json
{
  "query": "frameworks similar to React but with stronger type safety",
  "provider": "exa",
  "search_type": "neural"
}
```

```json
{ "query": "claude 4 release notes", "provider": "tavily", "search_type": "news" }
```

```json
{ "query": "site:reddit.com vim plugins 2025", "provider": "serpapi" }
```

## Pair with `web_fetch`

Two calls per question:

1. `web_search { "query": "..." }` — get a candidate URL.
2. `web_fetch { "url": "..." }` — read the page.

Tavily / Exa snippets often answer simple questions without the second
call.

## When to skip search

- **You already have the URL.** Just `web_fetch`.
- **An MCP server has the right access.** Calling a Linear / GitHub /
  Notion MCP is usually better than searching the public web for the
  same data — authenticated, structured, reliable.
- **Math / reasoning / explanation questions.** Don't waste budget on
  search when you know the answer.

## Failure modes

- **No API key configured.** Validation error naming the four env vars
  (`TAVILY_API_KEY`, `EXA_API_KEY`, `BRAVE_SEARCH_API_KEY`,
  `SERPAPI_API_KEY`). Surface to the user via `send_message`.
- **Quota / 429 / 5xx.** Internal error with HTTP status + provider
  message. Retry with `provider: <other>` when multiple keys exist.
- **Egress allow-list blocks the host.** Same connection error as
  `web_fetch`. Ask the operator to add the provider host to
  `container_configs.egress_allow`.
