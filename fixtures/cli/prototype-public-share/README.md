# cli/prototype-public-share (M19 A3 / X-rider W3)

Capability fixture for the **A3 public-tunnel verb** (`make_preview_public`) and
its ritual-card surface.

## What it exercises

A user asks the agent to "share it publicly". The scripted 4-round tool loop:

1. `expose_preview {port: 8000}` — the LAN preview (serviced by the harness's
   `FixturePreviewBroker`, returns the `http://192.0.2.10:8100/__preview/...`
   LAN URL, exactly as `prototype-golden` does).
2. `make_preview_public {port: 8000}` — the A3 public verb, relayed through the
   SAME reserved `__preview` server but routed host-side to the harness's
   `FixtureTunnelBroker`, which returns a **public** `https://fixture-tunnel.example/...`
   URL (modeling the post-approval reply).
3. `send_card { ... }` — the prototype-ready **ritual card**, which now carries
   an **"Open the public link"** button pointing at the public URL, alongside
   the LAN "Open preview" button.
4. A closing `send_message` summary.

The X-rider assertion (`cli_prototype_public_share_ritual_card_has_public_button`
in `tests/replay.rs`) pins the delivered ritual card's public-URL button shape
on top of the byte-stable JSONL diff.

## Gates

`gates: ["preview", "tunnel"]`:

- `"preview"` wires `FixturePreviewBroker` + advertises the M17 preview verbs
  (as `prototype-golden` does).
- `"tunnel"` (new, X-rider W3) wires `FixtureTunnelBroker` via the already-public
  `DeliveryService::set_tunnel_broker`, so `make_preview_public` gets a canned
  public URL instead of the "no tunnel support wired" error. The
  `make_preview_public` tool is already advertised by `preview_tool_defs()` and
  routes via `run::preview::is_preview_tool`, so the tunnel gate only supplies
  the host-side broker.

## Deliberately out of scope

- **The approval round-trip.** `make_preview_public`'s first-call
  pending-approval → operator tap → second-call live-URL flow is a host-handler
  concern, covered by A3's `copperclaw-host` `preview.rs` tests and the
  `copperclaw-modules` `tunnel.rs` module tests. The `FixtureTunnelBroker`
  returns the `Exposed` reply directly (the post-approval state).
- **Real cloudflared / a real container IP / port mapping.** The broker is a
  canned test double, like `FixturePreviewBroker`.
