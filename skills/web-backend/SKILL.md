---
name: web-backend
description: The golden path for an app that needs a server, an API, and a real database — choosing the right datastore (not just SQLite), designing HTTP endpoints that don't 500 on the caller, and laying out backend + frontend as separate, wired-together pieces. Use whenever a build goes past static files — "wire it to a database", "add an API/backend", "persist data", "make it API-backed", auth, or multi-user state.
---

# web-backend

The moment an app needs to *persist* data across reloads or serve more than
one user, it needs a server + a datastore. Don't bolt a database onto a pile
of frontend files — lay the backend out as its own piece and wire the two
together. This skill is the backend counterpart to [[web-app-scaffold]] (the
frontend scaffold) and plugs into [[coding-task]]'s verify gate + delivery.

## 1. Pick the datastore deliberately — it is not always SQLite

Choose by the shape of the data and who writes it, then commit:

- **SQLite** — the default for a single-node prototype: embedded, zero-setup,
  one file. Correct for a demo one operator drives. Caveats on this runtime:
  the DB sits on the bind-mounted `/data` and the container can be
  hard-killed, so **use the default rollback journal (`journal_mode = DELETE`),
  never WAL** — a killed WAL truncates to an unrecoverable
  `SQLITE_IOERR_SHORT_READ`. Keep the file under `/data/<project>/data/`.
- **Postgres** — reach for it when the app has concurrent writers, real
  relational integrity, multiple clients, or is meant to look production-shaped.
  Run it as a service the app connects to over `DATABASE_URL`. If the image has
  no `psql`/server, say so and either bake it (`install_packages`) or fall back
  to SQLite with a note — don't silently hand-roll a fake.
- **A JSON/flat file** — fine only for tiny, single-writer config/state. If you
  find yourself writing query/filter logic by hand over a JSON blob, you needed
  a database; stop and use one.
- **Redis / a KV store** — only for cache/session/ephemeral data, alongside a
  real DB, never as the system of record for a prototype.

Rules that hold for every choice:

- **Config from the environment, never hardcoded.** Read `DATABASE_URL`
  (or `DB_PATH`) from `process.env` with a sane local default; commit a
  `.env.example`, never a real secret.
- **Schema + seed are scripts, not side effects.** A `migrate`/schema step and
  a separate `seed` step (idempotent — `CREATE TABLE IF NOT EXISTS`, upserts)
  that you can re-run. The app must not depend on data that only exists because
  you typed it in once.
- **One data-access layer.** All SQL/queries live in a `db`/`repo` module the
  routes call — routes never inline SQL. This is what lets you swap SQLite for
  Postgres later without touching handlers.

## 2. Design the API so the caller never eats a raw 500

An endpoint that throws a bare 500 (or an unhandled promise) is why a UI shows
"Internal server error" with nothing to act on. Design against that:

- **Resource-oriented routes.** `GET/POST /api/things`, `GET/PATCH/DELETE
  /api/things/:id`. Nouns, not verbs; plural collections.
- **One consistent JSON shape.** Decide on `{ data }` / `{ error }` (or an
  envelope) and use it everywhere, so the frontend has one thing to parse.
- **Correct status codes.** 200/201 on success, 400 on bad input, 401/403 on
  auth, 404 on missing, 409 on conflict, 500 ONLY for a genuine bug.
- **Validate every input at the boundary** — required fields, types, lengths —
  and return 400 with a message, before it reaches the DB.
- **Catch and shape errors.** Wrap handlers so a thrown error becomes a logged,
  structured JSON error with the right status — never a leaked stack trace and
  never a hang. Add a top-level error handler as the backstop.
- **A `GET /api/health`** that checks the DB connection — the fastest "is the
  backend actually up" probe, for you and for verify.
- **CORS / same-origin.** In dev the frontend calls `/api` and the dev server
  proxies it (see §4); if the browser hits the API cross-origin, enable CORS
  explicitly.

## 3. Lay out backend and frontend as separate pieces

Not one god-file. A clean prototype layout:

```
/data/<project>/
  src/            # frontend (vite) — see [[web-app-scaffold]]
  server/
    index.ts      # app + route wiring + listen(PORT)
    db.ts         # connection + schema, the ONLY place that opens the DB
    routes/       # one file per resource (things.ts, users.ts)
    seed.ts       # idempotent seed script
  .env.example    # DATABASE_URL / PORT / etc. — no secrets
  API.md          # the endpoints, one line each (contract the frontend reads)
  package.json    # scripts: dev, dev:api, dev:full, build, test, seed
```

Keep the server's port in `PORT` (env, default e.g. 3001) so it never collides
with vite's. Write `API.md` as you add routes — it's the contract, and it keeps
the frontend and backend honest.

## 4. Wire the frontend to the backend (dev)

Run both, and proxy the API so the browser only ever talks to one origin:

- **vite proxy:** in `vite.config.ts`, `server.proxy` maps `/api` →
  `http://localhost:<PORT>`. The frontend fetches relative `"/api/..."` (never a
  hardcoded `http://localhost:3001`), so it works behind the proxy and the
  [[preview]] tunnel unchanged.
- **Run both:** a `dev:full` script (`concurrently -n api,web "npm run dev:api"
  "npm run dev"`) so one command brings the whole stack up. Start it in the
  background so the turn doesn't block, then `ui_screenshot` the app.
- If the UI shows an API error, check the **server** job log first (the API
  process), not the browser — a 500 is almost always the backend or the DB.

## 5. Verify the backend too

Extend `.copperclaw/verify` (per [[coding-task]]) past lint/typecheck:

```
typecheck: tsc --noEmit
test: npm test            # unit-test the data layer + a couple of route handlers
```

Test the data-access layer and at least the create/read path of each resource
against a throwaway DB — that's what proves persistence actually works before
you claim it does. See [[testing]] for reading a failing stage's tail.

## Related skills

[[web-app-scaffold]] (the frontend half this pairs with), [[coding-task]] (the
verify gate + delivery ritual), [[testing]] (verify output), [[preview]]
(serving the running stack), [[debug]] (chasing a 500 to its cause).
