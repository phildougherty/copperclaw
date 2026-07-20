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
   - **Vite** (the common one): `npm run dev` alone binds `localhost` and the
     preview 500s with "the app did not respond". Set `server.host: true` **and**
     `server.allowedHosts: true` in `vite.config.ts` (Vite 6 blocks the proxied
     Host header) — or run `vite --host 0.0.0.0`. The `/api` proxy still targets
     `localhost:<api-port>` inside the container; only the dev server's own bind
     needs `0.0.0.0`.

2. **Confirm it is actually listening** before exposing it, e.g.
   `curl -s -o /dev/null -w '%{http_code}' http://localhost:<port>/`
   via `shell`. Exposing a dead port gives the operator a broken link.

3. **Call `expose_preview`** with the port your server listens on
   (and an optional short `name`). It returns a URL plus a validity
   note.

4. **Send the returned URL to the user verbatim, as copyable text.** Do
   not shorten, rewrite, or invent a URL — the token in the link is what
   grants access, and only the exact string the tool returned works. Put
   the FULL link (scheme + `:port` + `/__preview/<token>`) in the message
   body as plain text (not only a card button), so the operator can
   long-press → Copy Link and open it in any browser. Tell them: the link
   works in Safari/Chrome too, but they must open THIS whole link — the
   bare domain (what the address bar or a share sheet shows after the
   redirect) 403s because it has no token. Include the idle-expiry note.

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

## Sending it beyond the LAN (`make_preview_public`)

`expose_preview` gives a link that only works on the operator's own
network. When the operator wants to **share it with someone off their
network** — "send it to my cofounder", "give me a link for the client" —
use `make_preview_public` to publish the *same live preview* to the public
internet through a tunnel.

1. **Expose it on the LAN first.** `make_preview_public` fronts a preview
   that is already live, so `expose_preview` the port first (and confirm it
   works), then call `make_preview_public` with the **same container port**.

2. **It always needs operator approval.** The first call returns a
   "pending approval" note and posts an approval card — no public link is
   created yet. Tell the operator to tap Approve, then **call
   `make_preview_public` again**; the second call returns the shareable
   public URL. This is not an error — it is the safety gate; every public
   exposure requires a human tap.

3. **Relay the public URL verbatim and put it on your delivery card.** Send
   the exact URL the tool returned, and add it to your "prototype ready"
   `send_card` as a button, e.g.
   `{ "label": "Open the public link", "url": "<the URL>" }` — that button
   is the share-with-a-cofounder hand-off. Say plainly that it is a
   **public** link (anyone with it can reach the app).

4. **It tears down with the preview.** When you `close_preview` (or the
   preview goes idle), the public tunnel is torn down automatically — you do
   not close it separately. Re-sharing later needs a fresh approval.

**Errors and how to react**

- **"Public tunnels are OFF"** / **"not available on this host"** — the
  operator has not enabled the public-tunnel capability. Relay that they
  must set `COPPERCLAW_PUBLIC_TUNNEL_ENABLED` and have previews enabled;
  the LAN `expose_preview` link still works in the meantime.
- **"tunnel binary … was not found"** — cloudflared is not installed.
  Relay the copy-pasteable install steps the error gives you, then retry.
- **"no live preview on container port N"** — you have not exposed that
  port yet (or it lapsed). `expose_preview` it, then retry.

Do not reach for `make_preview_public` for the operator's own testing — a
LAN `expose_preview` is enough. Only make a preview public when the
operator explicitly wants to share it beyond their network.

## Preview as the demo hand-off

A live preview link is the strongest way to end a build the operator can
try from their phone. When you finish an HTTP prototype, `expose_preview`
it and send the URL as *part of your delivery*, alongside the downloadable
artifact (`send_file`) and the `artifact_path` host path — see
[[coding-task]] and [[send-file]]. That file-plus-path-plus-link trio is
the "prototype ready" close. If the operator wants to share it beyond their
network, `make_preview_public` (above) adds a public-URL button to that
close card after an approval tap. `close_preview` once the operator has
seen it (that also tears down any public tunnel).

## Limits to keep in mind

- The preview expires after 30 minutes idle; re-expose if it lapsed.
- Anyone on the operator's network who has the link can open it, so
  do not put secrets in the app you expose.
- WebSockets are proxied: Vite dev servers, live reload, and realtime
  apps work through the preview link the same as plain HTTP. Build with
  sockets when they fit the app — no need to fall back to polling.
