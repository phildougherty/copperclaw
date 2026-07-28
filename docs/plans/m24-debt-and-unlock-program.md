# M24 — Truth, security debt, and unlock program

## Context — why this milestone

A three-agent audit of every plan doc (m17-m23, slice-3, vaporware
followups, PLAN.md) against current `main` found the milestone programs
shipped nearly complete — but the residue clusters into three kinds of
debt, each verified against code, not prose:

1. **Lies in the manual**: skills and docs that teach the opposite of
   what the code does (worst: `skills/install-packages` denies the
   session-scope install that `self_mod.rs` shipped in M18 E1).
2. **Security follow-ups written down and never revisited**: the M19 A3
   tunnel trio, the advisory-only `unknown_sender_policy`, unreachable
   grant revocation, the unwired fresh-approval taint clearance.
3. **Features built but unreachable**: the M22 skill-relevance selector
   with no config write path, `daily_cost_cap` echoed but never
   enforced, images flattened to `"<image>"` across agent boundaries,
   agent-to-agent addressing stopped at phase 1 of 4.

## Wave 1 — truth (copy and comments only, no behavior)

- **T1 Skill copy vs `install_packages` reality.** `self_mod.rs` accepts
  `scope: "session"` (pip/npm usable this turn) and routes effects
  through an approval row; `skills/install-packages` still teaches the
  pre-E1 schema, claims "no approval gate", and says an in-session
  install is impossible ("you will wait forever") — and
  `skills/databases` + `skills/coding-task` propagate the claim. Read
  the tool source, rewrite the copy to match, keep caps/conventions.
- **T2 Phantom capture pipeline.** `docs/replay-fixtures.md:235-241`
  instructs operators to use `COPPERCLAW_FIXTURE_CAPTURE` and a
  `fixture/redact.rs` that have never existed. Rewrite the section to
  document the real authoring flow (hand-authored fixtures + the
  `COPPERCLAW_*_GENERATE` env paths the replay harness actually
  supports). Implementing capture stays a possible M25 card.
- **T3 Stale anchors.** Remove the deleted-`DISALLOWED_TOOLS` references
  in `container_manager/runner_config.rs` and
  `copperclaw-db/src/tables/container_configs.rs`; fix
  `m18-prototype-builder-program.md`'s stale "V5 HELD" row and closing
  line (PR #55 merged 2026-07-16); clarify `run/lsp.rs` doc text so
  `Backend::LanguageServerAssisted` reads as what it is (a recorded
  server hint over a ctags index), comments only.

## Wave 2 — security debt (LANDED)

All four cards landed together: S1 nonce-keyed tunnel approvals with
consume-before-stand-up, legible cards, the `preview_enabled` kill
switch, and the conditional `Secure` cookie; S2 enforcing the stored
`unknown_sender_policy` value set (`open` admits; everything else,
including unrecognized values, stays pending); S3 `grants.list`/
`grants.revoke` + `cclaw grants` with eager snapshot withdrawal; S4 the
full tappable-card taint-clearance loop (request row -> pending
approval + card -> single-turn session-scoped clearance file,
consume-and-delete). Details per card below.

- **S1 Tunnel hardening (M19 A3 trio + cookie).** (a) Key the
  public-tunnel approval on a per-request nonce instead of the
  pool-reused `(session, host_port)` and make grant consumption
  fail-closed before stand-up (`modules/src/tunnel.rs`); (b) name the
  app/port/session in the approval card; (c) make
  `preview_enabled=false` tear down the group's live tunnels
  (`handlers/groups.rs`); (d) add `Secure` to the preview cookie when
  fronted by HTTPS (`preview.rs`).
- **S2 `unknown_sender_policy` enforced or removed.** The gate closure
  in `modules/src/approvals.rs` never reads the stored policy. Implement
  the stored semantics in the gate (with tests per policy value) —
  removal-by-migration is the fallback if the semantics turn out
  incoherent; decide from the M17 E4 card text.
- **S3 Grant revocation surface.** `task_grants::revoke` has zero
  production callers. Wire a host handler + `cclaw grants list|revoke`
  (audited, metrics: the reserved `inc_task_grant("revoked")` outcome).
- **S4 Fresh-approval taint clearance.** `external_approved` is
  hard-wired false while policy text promises a fresh-approval route.
  Design and wire the minimal live path: taint-blocked action emits an
  approval request; operator approval writes a session-scoped,
  single-turn clearance the runner reads on its next turn. Reuse the
  existing approvals + grant-snapshot plumbing; do not invent a new
  approval system.

## Wave 3 — unlock (LANDED)

All four cards landed: U1 `skills` config field + `cclaw groups skills`
sugar with registry validation and an e2e narrowed-prompt test; U2
cost-cap enforcement at the spawn gate + broker verdict (unpriced spend
counted separately, never silently zero; `budgets.set` cost-cap wipe
bug fixed); U3 save-and-reference image passthrough at the subagent and
external-MCP seams (5 MB / 20-file caps, degrade-never-error); U4
`to: "user"` / `to: "agent:parent"` via the spawn-materialized
session-routing chain (no new tables), skill copy, and the parent chain
in `sessions.get` + `cclaw sessions list`. One correction to the card
text below: the per-session `destinations` table the U4 card suggested
extending has no live host writer — `session_routing` is the real
materialized seam, and the implementation uses it. Details per card
below.

- **U1 Skill-relevance config surface.** Expose
  `container_configs.skills` through `groups.config_update` and
  `cclaw` so `SkillsSelector::Relevant`/`Explicit` are reachable;
  document in `docs/container-config.md`.
- **U2 `daily_cost_cap` enforcement.** Compare rolled-up spend against
  the cap in the same gate as `daily_token_cap`
  (`container_manager/budgets.rs`), surface in `cclaw budgets`/doctor.
- **U3 Image passthrough across boundaries.** Stop flattening
  `RawContent::Image` to `"<image>"`: save to a capped session-dir file
  and hand off via the `view_image` path, in both the subagent
  transcript seam (`runner/src/subagent.rs`) and the external-MCP
  render seam (`host-delivery/src/service.rs::render_mcp_content`).
- **U4 Agent-to-agent addressing phases 2-4.** Add `to: "user"` and
  `to: "agent:parent"` recipient forms (`mcp/src/context.rs`,
  `runner/src/tools.rs`), teach them in `skills/send-message` +
  `skills/create-agent`, and surface `source_session_id` in
  `sessions.get` / `cclaw sessions`.

## Deferred to M25 (audited, real, not this program)

Reaction parity beyond 4 channels; native-vs-fallback delivery metrics
(slice-3 Q5); `web_fetch` pagination; multi-provider search fan-out;
`memory_delete`; the m17 D-series operator-UX items (`--watch`,
pickers, rustyline REPL, `logs` filters, `sessions clear`,
`briefing edit`, guided restore, Slack/Discord wizard steps); default
ports for line/mattermost/webhooks; slice-3 Q4 edit-horizon guard;
implementing fixture capture; cutting the first release tag.
