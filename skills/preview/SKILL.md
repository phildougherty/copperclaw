---
name: preview
description: Let the operator try an HTTP app you built, live in their browser. Serve it on 0.0.0.0 inside the container, expose it with `expose_preview`, send them the returned URL verbatim, and `close_preview` when they are done. Use whenever the user asks to try, test, open, or see something you built that serves HTTP.
---

# preview

You run inside a container with no published ports, so the operator
cannot reach a server you start directly. The `expose_preview` tool
asks the host to stand up a token-gated reverse proxy from a port on
the operator's machine to your server, and gives you back a shareable
URL.

## The flow

1. **Serve on `0.0.0.0`, not `127.0.0.1`.** The host's proxy dials
   your container's bridge IP, so a server bound to loopback inside
   the container is NOT reachable. Examples:
   - `python3 -m http.server 8000 --bind 0.0.0.0`
   - `npx serve -l tcp://0.0.0.0:3000`
   - Express: `app.listen(3000, "0.0.0.0")`
   - Flask: `app.run(host="0.0.0.0", port=5000)`

2. **Confirm it is actually listening** before exposing it, e.g.
   `curl -s -o /dev/null -w '%{http_code}' http://localhost:<port>/`
   via `shell`. Exposing a dead port gives the operator a broken link.

3. **Call `expose_preview`** with the port your server listens on
   (and an optional short `name`). It returns a URL plus a validity
   note.

4. **Send the returned URL to the user verbatim.** Do not shorten,
   rewrite, or invent a URL — the token in the link is what grants
   access, and only the exact string the tool returned works. Include
   the note about idle expiry.

5. **Call `close_preview` with the same port** when the user says
   they are done, asks you to take it down, or the task is finished.

## Errors and how to react

- **"Preview is not enabled for this agent group"** — the operator has
  not opted this group in. Relay the exact command the error message
  gives you (it names the group id); do not paraphrase it away.
- **"no reachable network address"** — your container has no bridge
  IP (or the server is not up). Check the server is running and bound
  to `0.0.0.0:<port>`, then retry.
- **"Preview capacity reached"** — close a preview you no longer need
  with `close_preview`, then retry.

## Limits to keep in mind

- The preview expires after 30 minutes idle; re-expose if it lapsed.
- Anyone on the operator's network who has the link can open it, so
  do not put secrets in the app you expose.
- WebSockets are not proxied in this version — plain HTTP requests
  and streamed responses only. Prefer polling over sockets in demo
  apps you build for preview.
