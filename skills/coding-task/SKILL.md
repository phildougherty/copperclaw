---
name: coding-task
description: Disciplines for doing real coding work — editing files, running tests, verifying that what you built actually works, and delivering the artifacts so the operator can use them. Opt-in via `SkillsSelector::Explicit` on the agent group; non-coding agents should not enable this.
---

# coding-task

How to do coding work as a Copperclaw agent. The base session image
ships `python3`, `pip`, `node`, `npm`, `git`, `curl`, `wget`, `jq`,
`build-essential` via `shell`; the *prototyping* profile also bakes
`typescript`, `eslint`, `prettier`, `tailwindcss`, `ruff`, `sqlite3`,
`create-vite`/`vite` — probe first (`command -v eslint`).

Need a pip/npm package right now? `install_packages` with
`scope: "session"` installs it into `/data` this turn. A toolchain on
neither (Go, Rust, a JVM): untar the official build into `/data` and
extend `PATH` — no root, no apt. See [[install-packages]].

## Every project is a git repo (do this first)

```bash
mkdir -p /data/<project> && cd /data/<project>
git init && git add -A && git commit -m "init: scaffold"
```

One repo **per project directory** under `/data` (chat clears between
projects, `/data` doesn't). Commit after each working increment, not
just the end. `create_agent` siblings only get a WRITABLE worktree
(see [[create-agent]]) of the repo you're `cd`'d into — outside a
repo they drop to read-only. An existing checkout: use it as-is.

## Changing an *existing* repo — clone, it auto-attaches

Changing an existing codebase, not building one? Clone with `shell` git
(no clone/commit tool — plain git, same egress guard): `cd /data && git
clone <url> <project>`. Next turn the runtime auto-attaches any cloned
`/data` repo (has an `origin` remote a prototype lacks): it **infers
`.copperclaw/verify` stages** from its manifests (Cargo, `package.json`
scripts, Makefile, pyproject) so the verify gate below covers it from
turn one, seeds `.copperclaw/DECISIONS.md`, and marks it attached.
Refine the inferred `.copperclaw/verify` — you own it after attach.

## Decompose before you build

Multiple moving pieces (frontend + API + DB, services)? Run
[[architecture]] first — requirements, seams, data model, DECISIONS.md
— then decompose files inside those components.

Name the modules/files and each one's single responsibility BEFORE
writing code — "build X" as the whole plan becomes a god-file nobody
can safely edit in parallel.

- **One responsibility per file.** `auth.py` does auth, not auth *and*
  logging *and* config — "and" in a file's description means split it.
- **Module boundaries follow what changes together**, not arbitrary
  size. Split on job collision (two features fighting over one file),
  not line count.
- Put the module list in your first `todo_add` batch — the plan, not a
  mental note.
- **Read before you write**, match the surrounding style; prefer
  editing existing files; no drive-by cleanup; comment only the
  non-obvious *why*.

## Choosing dependencies: baked beats fetched beats hand-rolled

1. **Baked first.** `create-vite` scaffolds a real project — see
   [[web-app-scaffold]] for the full golden path (skip hand-rolled
   `index.html` + script tags); `typescript` once scaffolded;
   `sqlite3` for real storage, not a hand-rolled JSON "database".
2. **Fetched second.** `npm install <pkg>` / `pip install <pkg>` for
   edge cases already solved upstream (dates, markdown, password
   hashing) — don't hand-roll bcrypt.
3. **Hand-rolled last**, only for glue logic specific to this app.

- **Databases & APIs:** load [[web-backend]] when an app needs a server,
  API, or persistence — datastore choice (not always SQLite), endpoint
  design, layout; [[databases]] runs a real DB server in-container.
  Inline rule: a `/data` SQLite DB uses
  `journal_mode = DELETE`, never WAL (a killed WAL corrupts).

## Robustness: handle what a user can actually hit

Skip handling only for genuinely impossible inputs — "impossible"
means **no code path can produce it**, not merely unlikely. A
user-typed / user-submitted / wrong-usage path is in scope, even in a
prototype:

- **Bad input** — empty string, wrong type, out-of-range. Fail at the
  boundary with an actionable message, not a stack trace.
- **Empty state** — zero-item list, no-results search, no data yet.
  Design it; don't render a blank screen.
- **Network/IO failure** — timed-out fetch, missing file, denied
  write. Catch it, show something; don't crash over one bad request.

Out of scope: a null only your call sites pass, a format you control.

## Verify before you claim done — `.copperclaw/verify` (NOT OPTIONAL)

"Production-ready" / "complete" / "working" are claims about evidence,
not vibes. Run it — `python3 x.py` (exit 0), `node x.js` + `curl` for a
server, `pytest`/`npm test` for tests ("it compiles" is not the bar) —
via the project's *canonical* build (`cargo build`, `go build ./...`,
`npm run build`), never an ad-hoc per-file check. Couldn't run it? Say
so — that beats a fabricated "done".

Verification is also *enforced*. Write
`/data/<project>/.copperclaw/verify` **at scaffold time**, one stage
per line (an optional `name:` prefix names it; an unprefixed line gets
`stage1`, ...):

    lint: npx eslint .
    typecheck: tsc --noEmit
    test: npm test

**Probe before writing a stage that needs a tool** (`command -v
eslint`) — an absent binary just fails the stage cold. `check_command`
(per-group config) overrides the whole file with one command.

Any edit marks the project **dirty**, resetting every stage. While
dirty, `todo_update(status="completed")` refuses, naming the failing
stage(s), the exact command, and fix-cycles left. Clear a stage by
running **exactly** its command via `shell` with `cwd` set to the
project — exit 0 records green, nonzero records a fix cycle (named in
`last_failure`); **2** failures auto-block the todo. Every stage must
be green since the last dirty mark. Truncated log tail:
[[testing]]/[[debug]] (`tail_bytes`, paged `read_file`).

## See it, then fix it — before the delivery todo

Any build with a UI runs this loop at least once — skip only when
there is genuinely no UI to look at:

1. After the first visual milestone, `ui_screenshot` the running app.
2. **Look** at the image — don't just note the call succeeded.
3. `load_skill("frontend-design")`, run its `## Critique checklist`.
4. Fix the worst two findings, then `ui_screenshot` to confirm.

One full cycle before the delivery todo. See [[web-app-scaffold]].

## Delivering artifacts to the operator

Files under `/data/` are invisible to the operator unless you do one
of these — pick one per artifact: **`send_file`** (small files),
**`artifact_path`** (host `/data` path, paste verbatim; many-file
projects), or a live preview link (`expose_preview`, send
the URL verbatim — see [[preview]]). Without one, you've delivered
nothing usable — `/data` is the *container*'s path, not theirs.

**End every build with the "prototype ready" close.** Last todo: ONE
`send_card` — title, one-line summary, a "What to try" bullet, an
**Open preview** `url` button (exact `expose_preview` URL, HTTP apps
only), a **Download** button (`value: "download"` — ships
the `git archive` zip via `send_file` next turn), `artifact_path` in a
footer field, and the screenshot via its own `send_file`. Degrades by
capability — no preview → no button, never a dead link. Full card:
[[send-card]]; zip: [[send-file]].

## Don't fabricate

If `web_search` got 12 results, say "12 results" — never invent
numbers. **Code fabrication is the same sin, worse:**

1. **Never mark a todo `completed` for code you didn't write** —
   confirm with `git_status` / `glob` / `read_file` first; if not
   there, stay `in_progress` and say so.
2. **Never document code that doesn't exist yet.** Build it first.
3. **Never reference a directory that doesn't exist** in a compose
   file / Makefile — vapor fails on a fresh checkout.
4. **"Done" means the artifact is on disk and passes `ls`**, and any
   claimed commit shows up in `git log`.

## Knowing when to stop

Match the change to what was asked — no drive-by refactors. Don't
half-finish: if you can't complete in one pass, stop and say what's
left.

## Related skills

- [[architecture]], [[git-commit]], [[code-review]], [[testing]],
  [[todo-tracker]], [[agent-memory]], [[install-packages]],
  [[frontend-design]], [[web-app-scaffold]], [[databases]]
