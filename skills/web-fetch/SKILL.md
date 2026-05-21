---
name: web-fetch
description: HTTP GET or POST a URL from inside the container with the web_fetch tool, including the body cap, timeout ceiling, and egress allow-list interaction.
---

# web-fetch

`web_fetch` performs an HTTP GET (or POST) from inside the session
container and returns the response. It exists so the agent has a
URL → body pipe without needing to invoke `curl` through `shell`.

There is no `web_search` companion (yet) — `web_fetch` requires you
to already know the URL. To discover URLs, ask the user, consult an
MCP server (e.g. the `linear` preset for project context), or use
`shell` with a search tool you've installed via `install_packages`.

## Schema

```json
{
  "url": "string, non-empty",
  "method": "string (optional, GET or POST, default GET)",
  "body": "string (optional, only for POST)",
  "timeout_secs": "integer (optional, max 120)"
}
```

- `url` (required). Must include the scheme (`https://...` or
  `http://...`).
- `method` (optional). `GET` (default) or `POST`. Other verbs return
  a validation error — use `shell` with `curl` for PATCH / PUT /
  DELETE if you really need them.
- `body` (optional). For `POST`, the request body. Sent as
  `application/json` content type when the body parses as JSON,
  otherwise as `text/plain`.
- `timeout_secs` (optional). Default 30s, ceiling 120s.

## Output limits

The response body is capped at 256 KiB. Larger responses are
truncated at a UTF-8 character boundary (binary responses are
returned base64-encoded; the truncation is still on byte
boundaries). The result includes `truncated`, `bytes_read`,
`total_bytes` so you can tell.

## Result shape

```json
{
  "url": "https://api.example.com/v1/items",
  "status": 200,
  "headers": { "content-type": "application/json", "...": "..." },
  "body": "...",
  "bytes_read": 1421,
  "total_bytes": 1421,
  "truncated": false,
  "elapsed_secs": 0.31
}
```

## Egress allow-list interaction

Operators can restrict the container's outbound network via
`container_configs.egress_allow` (see `docs/container-config.md`).
When set, `web_fetch` calls to hosts outside the allow-list fail
with a connection error — the tool itself doesn't pre-validate; it
trusts the runtime's network policy.

If you see a network error on a URL that "should" work, the cause
is most likely the egress allow-list. Ask the operator to add the
host or use a different approach (an MCP server with the right
access already wired up is often the better path).

## When to prefer other tools

- **Talking to an authenticated API**: an MCP server with the right
  credentials (`iclaw mcp add linear ...`) is almost always
  cleaner than threading API keys through the agent.
- **Fetching the same URL many times**: cache the first result in
  conversational memory rather than re-fetching.
- **Downloading a large file**: use `shell` with `curl -o <path>`
  to stream straight to disk without going through the model's
  context window.

## Common patterns

Fetch JSON and reason over it in the same turn:

```json
{ "url": "https://api.github.com/repos/anthropics/claude-code/issues?state=open&per_page=5" }
```

POST a webhook with a small JSON payload:

```json
{
  "url": "https://hooks.example.com/notify",
  "method": "POST",
  "body": "{\"event\": \"deploy\", \"status\": \"green\"}"
}
```
