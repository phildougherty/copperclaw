---
name: web-fetch
description: HTTP GET or POST a URL from inside the container with the web_fetch tool, including the body cap, timeout ceiling, and egress allow-list interaction.
---

# web-fetch

`web_fetch` does an HTTP GET (or POST) from inside the session
container and returns the response. URL → body pipe without `shell
curl`.

You need the URL already. To discover URLs, use `web_search` (see
[[web-search]]), ask the user, or consult an MCP server.

## Schema

```json
{
  "url": "string, non-empty",
  "method": "string (optional, GET or POST, default GET)",
  "body": "string (optional, only for POST)",
  "timeout_secs": "integer (optional, max 120)",
  "raw": "boolean (optional, default false)"
}
```

- `url` (required). Must include scheme (`https://...`).
- `method` (optional). `GET` (default) or `POST`. Other verbs are a
  validation error — use `shell curl` for PATCH/PUT/DELETE.
- `body` (optional, POST). Sent as the request body as-is; no
  Content-Type is set automatically and there is no headers field. If
  the server requires a specific Content-Type (most JSON APIs do), use
  `shell curl -H 'Content-Type: ...'` instead.
- `timeout_secs` (optional). Default 30s, ceiling 120s.
- `raw` (optional). True returns response body bytes unmodified for
  HTML. Default false converts HTML→markdown.

## HTML → markdown by default

When Content-Type is `text/html` / `application/xhtml+xml`, the body
auto-converts to markdown: strips `<script>`/`<style>`, preserves
links, formats headings + lists. Typically 5-10x smaller than raw HTML
— keeps your context window for content, not markup.

Response then includes:

- `body`: the markdown text.
- `content_type`: `"text/html → markdown"`.
- `raw_html_bytes`: original HTML size.
- `markdown_bytes`: converted size.

Pass `raw: true` when you need original HTML (scraping `<meta>`,
parsing embedded JSON, tag-level structure). JSON, plain text, and
non-HTML responses are returned as-is regardless of `raw`.

## Output limits

Response body capped at 16 KiB (~4k tokens). Larger responses truncate
at a UTF-8 char boundary with `truncated: true`. For full-page
extraction beyond the cap, `shell curl` and pipe through `head -c` /
`grep` for the section you need.

## Result shape

```json
{
  "url": "https://api.example.com/v1/items",
  "method": "GET",
  "status": 200,
  "content_type": "application/json",
  "body": "...",
  "size_bytes": 1421,
  "truncated": false,
  "elapsed_ms": 310
}
```

Full response headers are NOT surfaced (a single CSP / Set-Cookie
header can be tens of KiB) — only `status` and `content_type`. When
you need headers, use `shell curl -I`.

## Egress allow-list interaction

`container_configs.egress_allow` restricts outbound network. Off-list
hosts fail with a connection error — the tool doesn't pre-validate;
it trusts the runtime's policy. Separately, an SSRF guard rejects URLs
whose host resolves to loopback / link-local / private (RFC1918)
addresses before any socket opens, and re-checks every redirect hop.

Network error on a URL that "should" work → almost always the
allow-list. Ask the operator to add the host, or use an MCP server
with the right access wired up.

## When to prefer other tools

- **Authenticated API**: an MCP server (`cclaw mcp add linear ...`)
  beats threading API keys through the agent.
- **Same URL many times**: cache the first result in conversational
  memory.
- **Large file download**: `shell curl -o <path>` streams to disk
  without going through context.

## Common patterns

```json
{ "url": "https://api.github.com/repos/anthropics/claude-code/issues?state=open&per_page=5" }
```

```json
{
  "url": "https://hooks.example.com/notify",
  "method": "POST",
  "body": "{\"event\": \"deploy\", \"status\": \"green\"}"
}
```
