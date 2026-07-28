---
name: architecture
description: System-level design discipline for multi-component builds — requirements before structure, contracts at the seams, data ownership, recording decisions in DECISIONS.md, and right-sizing for what was actually asked. Opt-in for coding agents; use before writing code on anything bigger than one module.
---

# architecture

How to design a system before building it. [[coding-task]] covers
file-level decomposition; reach for this skill *before that*, whenever
a build has more than one moving piece (frontend + API + DB, a
pipeline, multiple services), when the user says "design", "architect",
or "how should this be structured", or when requirements are vague
enough that structure is a guess.

## 1. Requirements before structure

Write down, in the todo plan or a `DESIGN.md`, before any scaffold:

- **What it must do** — the 3-6 user-visible behaviours, as bullets.
- **The load-bearing qualities** — pick the two or three that actually
  matter here (durability? concurrent users? latency? auditability?)
  and design for those. A prototype for one operator has different
  correct answers than "production-shaped".
- **Your assumptions** — scale ("tens of users, not thousands"), what's
  out of scope, what you'll fake. State them so the user can correct
  them cheaply *now* instead of expensively later. If a single
  assumption would flip the whole design (multi-user vs single-user,
  realtime vs batch), ask that one question first — one
  `ask_user_question`, not a survey.

## 2. Draw the seams, then defend them

- **Name the components and one sentence of responsibility each.** If a
  component's sentence contains "and", split it. Fewer components is
  better: every seam you add must pay for itself.
- **Contracts at the seams.** Each seam gets a written interface —
  `API.md` for HTTP ([[web-backend]]), a schema for the DB, typed
  function signatures for in-process modules. Components depend on the
  contract, never on each other's internals.
- **Every datum has exactly one owner.** One component writes it;
  everyone else reads through that owner's interface. Two writers to
  one table/file is a design bug — fix it in the design, not with
  locks later.
- **Dependencies point one way.** UI → API → data layer → store. A
  cycle between components means the boundary is drawn wrong.

## 3. Data model first, endpoints second

Get the entities, their relationships, and their lifecycle
(created-by, mutated-by, deleted-when) down before routes or UI — the
data model is the part that hardens first and costs the most to
change. Then pick the store for the shape you drew: [[web-backend]]
for the decision rubric, [[databases]] to actually run Postgres /
MariaDB / Redis / Mongo in-container.

## 4. Record decisions — `.copperclaw/DECISIONS.md`

Every choice that would be expensive to reverse gets one line, at the
moment you make it: *what you chose, why, and the alternative you
rejected*.

    2026-07-28 SQLite over Postgres — single operator, zero-setup; revisit if multi-user lands.
    2026-07-28 Polling over websockets — 5s staleness acceptable per user; ws is the upgrade path.

Sessions compact and siblings spawn ([[create-agent]]) — this file is
how a future context avoids relitigating (or silently reversing) a
settled decision. Attached repos get it seeded automatically; keep it
current, it's the architecture's memory.

## 5. Right-size: simplest design that meets §1, with an exit

- Build for the stated requirements, not imagined ones. No queues,
  microservices, or caching layers a prototype doesn't need — that's
  resume-driven design.
- But leave the **exit**: the one-data-access-layer rule, env-var
  config, and contract-shaped seams are cheap now and are exactly what
  makes the upgrade (SQLite→Postgres, monolith→split) a swap instead
  of a rewrite. Note the upgrade path in DECISIONS.md instead of
  building it.

## 6. Stress the design before you build it

Sixty seconds of premortem against your component list: what happens
when the container respawns mid-write? When two requests hit the same
row? When the API is up but the DB is down? When input is 100x bigger
than expected? Each hole is either handled in the design, or written
down as an accepted risk — never discovered by the user. (Same
adversarial move as [[code-review]]'s pass, one level up.)

## Related skills

[[coding-task]] (file-level decomposition + verify gate),
[[web-backend]] (API + datastore rubric), [[databases]] (running real
DB servers), [[code-review]] (the adversarial pass), [[create-agent]]
(parallelizing along the seams you drew).
