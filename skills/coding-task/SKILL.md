---
name: coding-task
description: Disciplines for doing real coding work — editing files, running tests, verifying that what you built actually works, and delivering the artifacts so the operator can use them. Opt-in via `SkillsSelector::Explicit` on the agent group; non-coding agents should not enable this.
---

# coding-task

How to do coding work as a Copperclaw agent. The base session image
ships `python3`, `pip`, `node`, `npm`, `git`, `curl`, `wget`, `jq`, and
`build-essential` via `shell`. The *prototyping* profile also bakes
`typescript`, `eslint`, `prettier`, `tailwindcss`, `ruff`, `sqlite3`,
and `create-vite`/`vite` — probe first (`command -v eslint`), since not
every group runs that profile.

Need a toolchain in neither list (Go, Rust, a JVM)? Don't reach for
`install_packages` and wait — it only rebuilds the image for a
*future* session, so the binary never appears this turn. Download it
into `/data` instead (e.g. Go: `curl -fsSL <tarball-url> | tar -C
/data -xz`, then `export PATH=/data/go/bin:$PATH`) — no root, no apt.
See [[install-packages]].

## Every project is a git repo (do this first)

```bash
mkdir -p /data/<project> && cd /data/<project>
git init && git add -A && git commit -m "init: scaffold"
```

One repo **per project directory** under `/data` (chat clears between
projects, `/data` doesn't — never pile projects into one repo). Commit
after each working increment, not just the end. `create_agent`
siblings only get a WRITABLE worktree (see [[create-agent]]) of the
repo you're `cd`'d into — outside a repo they drop to read-only. An
existing checkout the operator points you at: use it as-is.

## Decompose before you build

Name the modules/files and each one's single responsibility BEFORE
writing code — "build X" as the whole plan becomes a god-file nobody
can safely edit in parallel.

- **One responsibility per file.** `auth.py` does auth, not auth *and*
  logging *and* config — a file description with "and" means split it.
- **Module boundaries follow what changes together** (storage, HTTP,
  UI), not arbitrary size.
- **Split on job collision, not line count.** Two features fighting
  over one file, split it; a 40-line script cut into three files is
  decomposition theater, not craft.
- Put the module list in your first `todo_add` batch — the plan the
  build follows, not a mental note.
- **Read before you write**, match the surrounding style; prefer
  editing existing files over new ones; no drive-by cleanup or
  speculative helpers; comment only the non-obvious *why*.

## Choosing dependencies: baked beats fetched beats hand-rolled

1. **Baked first.** `create-vite` scaffolds a real project (don't
   hand-write `index.html` + script tags past a single static page);
   `typescript` once scaffolded; `sqlite3` for real storage, not a
   hand-rolled JSON-file "database".
2. **Fetched second.** `npm install <pkg>` / `pip install <pkg>` for
   edge cases already solved upstream (dates, markdown, password
   hashing) — don't hand-roll bcrypt.
3. **Hand-rolled last**, only for glue logic specific to this app.

Probe before depending on an image tool (`command -v eslint`,
`python3 -c "import <pkg>"`) — an absent tool just fails cold.

## Robustness: handle what a user can actually hit

Skip handling only for genuinely impossible inputs — "impossible" means
**no code path can produce it**, not merely unlikely. A user-typed,
user-submitted, or wrong-app-usage path is in scope, even in a
prototype:

- **Bad input** — empty string, wrong type, out-of-range value. Fail
  at the boundary with a message the user can act on, not a stack trace.
- **Empty state** — zero-item list, no-results search, no data yet.
  Design it; don't render a blank screen.
- **Network/IO failure** — timed-out fetch, missing file, denied
  write. Catch it, show something; don't crash over one bad request.

Out of scope: a null only your own already-validated call sites pass,
a format you invented and control both ends of.

## Verify before you claim done — `.copperclaw/verify` and the gate (NOT OPTIONAL)

"Production-ready" / "complete" / "working" are claims about evidence,
not vibes. Run it — `python3 x.py` (exit 0), `node x.js` + `curl` for a
server, `pytest`/`npm test` for tests ("it compiles" is not the bar) —
using the project's *canonical* build (`cargo build`, `go build ./...`,
`npm run build`), never an ad-hoc per-file check. Couldn't run it? Say
so — "wrote X, couldn't run it, because Y" beats a fabricated "done".

Verification is also *enforced*, not just claimed. Write
`/data/<project>/.copperclaw/verify` **at scaffold time** — your first
edit, not a wrap-up step — as one or more independent stages, one per
line:

    lint: npx eslint .
    typecheck: tsc --noEmit
    test: npm test

An optional `name:` prefix (bare word + colon) names the stage; an
unprefixed line gets a derived name (`stage1`, ...) — a single
unprefixed line is exactly a one-command check, enforced the same way.
**Before writing a stage that needs a tool, probe for it**
(`command -v eslint`, `command -v tsc`) — only write stages for tools
confirmed on the image (or about to be installed this turn); an absent
binary just fails the stage cold. A per-group `check_command` config
overrides the whole file with one command.

Any edit (or non-verify `shell` command) marks the project **dirty**,
resetting every stage's result. While dirty, `todo_update(status=
"completed")` is refused, naming the missing/failing stage(s) + exact
command, plus fix-cycles remaining. Clear a stage by running
**exactly** its command via `shell` with `cwd` set to the project.
Exit 0 records it green; nonzero records a fix cycle and feeds the
failure back (`last_failure` names which stage broke). After **2**
failed cycles the todo auto-blocks instead of completing. Every stage
must read green since the last dirty mark, not just the one you most
recently ran. No file recorded → the error says so, write one. A
group that never touches a project never trips the gate. Reading a
truncated build log's END: [[testing]]/[[debug]] (`tail_bytes`, paged
`read_file`).

## Delivering artifacts to the operator

Files under `/data/` are invisible to the operator unless you do one
of these — pick one per artifact: **`send_file`** (channel-adapter
attachment, small deliverables), **`artifact_path`** (host-side path
for `/data`, paste it verbatim; many-file projects), or a live
preview link (`expose_preview` an HTTP server, send the URL verbatim —
see [[preview]]). Without one, you've built nothing the operator can
use — `/data` is the *container*'s path, not theirs.

**End every build with the "prototype ready" close.** Last todo is the
hand-off: ONE `send_card` — title, one-line summary, a "What to try"
bullet, an **Open preview** `url` button (exact `expose_preview` URL,
only if the app serves HTTP), a **Download** button (`value:
"download"` — the tap ships the `git archive` zip via `send_file` next
turn), `artifact_path` in a footer field, and the screenshot via its
own `send_file` when one exists (a card can't attach a local file).
Degrades by capability — no preview → no button, never a dead link.
Full worked card: [[send-card]]; zip idiom: [[send-file]].

## Don't fabricate

If `web_search` got 12 results, your report says "12 results" — never
invent stats or numbers you didn't compute.

**Code fabrication is the same sin, worse.** Concrete rules:

1. **Never mark a todo `completed` for code you didn't write** —
   `git_status` / `glob` / `read_file` first to confirm the files
   exist and hold the work; if not, stay `in_progress` and say so.
2. **Never document code that doesn't exist yet.** Build it, then
   document it.
3. **Never write a `docker-compose.yml` / `Makefile`** referencing a
   directory that doesn't exist — an artifact failing on a fresh
   checkout is vapor.
4. **"Done" means the artifact is on disk and passes `ls`**, and any
   claimed commit shows up in `git log`.

## Knowing when to stop

- Match the change to what was asked. No drive-by refactors.
- Don't half-finish. If you can't complete in one pass, stop and
  say what's left.

## Related skills

- [[git-commit]], [[code-review]], [[testing]], [[todo-tracker]],
  [[agent-memory]], [[install-packages]]
