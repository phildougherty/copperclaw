# M19 security-review records (A2, A3)

The M19 program tenet (plan rule 5) requires a recorded `security-review` pass
for every outward-facing surface before merge. The two outward-facing cards are
**A2** (interactive browser) and **A3** (public-tunnel verb). Each received an
independent adversarial review of its diff (not the whole program) by a
dedicated reviewer that read the load-bearing code paths directly. Verdicts and
follow-ups are recorded here for the operator merging the M19 branches.

## A2 — interactive browser (`browser_interact`) — SAFE WITH FOLLOW-UPS → fixed

Reviewed `b6e7c4d..card/a2`: `copperclaw-browser/{interactive,cdp,live,guard,container}.rs`,
`copperclaw-mcp/tools/{browser_interact,browser_render}.rs`, runner `policy.rs`.

Core machinery **PASS**: per-navigation SSRF re-guard runs after every action
and every redirect hop *before* the DOM is read (fail-closed on read failure);
dual opt-in (`COPPERCLAW_BROWSER_ENABLED` + `COPPERCLAW_BROWSER_INTERACTIVE`,
both required; tool not registered when off → read-only byte-stable); output
`Provenance::Untrusted` marks the turn untrusted; verbatim locked-down child
container (no broker/`ANTHROPIC_*` env, unprivileged user, deny-default egress)
with guaranteed teardown; action/text/timeout bounds.

Two findings, **both fixed in commit `112722a`** (A2 security follow-ups):

- **F1 (MEDIUM, authz):** `browser_interact` was not classified mutating, so a
  Guest sender in a Full-profile group could drive the write-capable browser.
  Fixed: added `BROWSER_WRITE_TOOLS` to `is_mutating` in
  `copperclaw-runner/src/policy.rs` (guest-denied), with a test. Kept out of
  `PROFILE_TOOL_LISTS` (conditionally-registered, like `browser_render`), so it
  stays Full-only.
- **F2 (LOW, js-injection):** the selector was spliced unescaped into a
  single-quoted `throw new Error('...')` literal in `cdp.rs`; a `'` in the
  selector could break into the page's own (already-untrusted) JS context — no
  host/egress reach. Fixed: the error message now concatenates the JSON-encoded
  (double-quoted) selector literal; injection test extended with a single-quote
  case.

## A3 — public-tunnel verb (`make_preview_public`) — SAFE TO MERGE

Reviewed `6d80c3f..card/a3` plus the underlying V5 module: `run/preview.rs`,
`policy.rs`, `copperclaw-host/preview.rs`, `host-delivery/service.rs`,
`modules/tunnel.rs`, `handlers/approvals.rs`, `boot.rs`.

All eight threat areas **PASS**, each test-backed: mandatory per-exposure
`CredentialedExternalAction` operator approval (no path to a public URL without
a human tap); one-shot grant bound to the approved payload (not the retry
request); taint- AND autonomy-gated (`make_preview_public` is in
`CREDENTIALED_EXTERNAL_TOOLS` and deliberately absent from `LAN_PREVIEW_TOOLS`);
default-off (host master switch `COPPERCLAW_PUBLIC_TUNNEL_ENABLED` ANDed with
per-group `preview_enabled`); the tunnel fronts the token-gated proxy with no
loopback/source-IP bypass (tokenless hit → 403); teardown on every
preview-death path (close / session-stop / shutdown / idle-tombstone, plus
`kill_on_drop`); guest-denied + Coding/Full-only; anonymous quick-tunnel, no
Cloudflare credential read/stored/forwarded, token never logged/audited; clean
actionable errors on absent binary / unwired broker.

Three **low-severity, non-blocking** follow-ups (recorded for a future
hardening pass; none gates merge):

1. `consume_grant` (`tunnel.rs`) is best-effort and host ports (8100-8199) are
   pool-reused within a session. If the revoke DB write ever fails *and* the
   same host port is reacquired in the same session for a different app, the
   still-`Approved` row keyed `tunnel:{session}:{host_port}` could re-stand-up a
   tunnel without a fresh tap. Hardening: make grant consumption transactional
   with stand-up, or key the request id on a nonce / approval-created-at rather
   than the reusable host port.
2. The approval card title is generic ("Expose this preview to the public
   internet?") and names neither the app nor the port; with several concurrent
   previews an operator can't tell which they're approving. Clarity, not a
   bypass (the grant is still bound to `(session, host_port)`).
3. Flipping per-group `preview_enabled` to false while a tunnel is live blocks
   new exposures but does not proactively tear down the existing one (it dies on
   close/idle/stop). Consider tearing down active tunnels for a group when
   previews are disabled.

Pre-existing (out of scope, M17): the preview gating cookie is
`HttpOnly; SameSite=Lax` without `Secure` — a minor weakness over the public
HTTPS tunnel, not introduced by A3.
