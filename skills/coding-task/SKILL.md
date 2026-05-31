---
name: coding-task
description: Disciplines for doing real coding work — editing files, running tests, verifying that what you built actually works, and delivering the artifacts so the operator can use them. Opt-in via `SkillsSelector::Explicit` on the agent group; non-coding agents should not enable this.
---

# coding-task

How to do coding work as an Copperclaw agent. The base session image
ships with `python3`, `pip`, `node`, `npm`, `git`, `curl`, `wget`,
`jq`, and `build-essential` available via `shell`. Use them.

## Doing the task

- **Read before you write.** Match the surrounding style.
- **Prefer editing existing files** over creating new ones.
- **No features the user didn't ask for.** No drive-by cleanup, no
  speculative helpers, no future-proofing, no error handling for
  impossible cases.
- **Comments only when the *why* is non-obvious.** Restating the
  code is noise.

## Verify before you claim done (NOT OPTIONAL)

"Production-ready" / "complete" / "working" are claims about
evidence, not vibes. Recipes:

- **Python script:** `python3 path/to/file.py`; confirm exit 0.
- **Node app:** `node path/to/file.js`; for an HTTP server, spawn it,
  `curl`, confirm a real response.
- **Tests exist:** run `pytest` / `npm test` / equivalent. "It
  compiles" is not the bar.

If you couldn't run it, **say so**. "I wrote X but couldn't run it
because Y" is honest. "Done" without evidence isn't.

## Delivering artifacts to the operator

Files you write under `/data/` are invisible to the operator unless
you do one of these. Pick one for every artifact you produce:

1. **`send_file`** — pushes the file through the channel adapter
   (Telegram shows it as an attachment, etc.). Good for small
   deliverables the operator wants on their phone.
2. **`artifact_path`** — returns the host-side filesystem path
   corresponding to `/data`. Include that path verbatim in your
   reply so the operator can `cd` to it. Good for many-file
   projects (entire repo, build output, etc.).

Without either, you've effectively built nothing the operator can
use. Saying "the files are at `/data/foo.html`" is wrong — `/data`
is the *container*'s path, not the operator's.

## Don't fabricate

If you used `web_search` and got 12 results, your report says "12
results." Never invent stats or numbers you didn't compute.

**Code fabrication is the same sin, and it's worse.** Concrete rules
to stop a class of failures we've seen in production:

1. **Never mark a todo `completed` for code you didn't actually
   write.** Before `todo_update(status: "completed")` on items like
   "Build backend X", "Implement Y API", "Create Z service" — run
   `git_status` / `glob` / `read_file` to confirm the files exist
   and contain the work. If they don't, leave the todo
   `in_progress` and either do the work or say "I'm out of time
   for this run."
2. **Never write documentation for code that doesn't exist yet.**
   `API_DOCUMENTATION.md` listing endpoints you haven't built is
   fabrication, not docs. `README.md` describing how to run a
   server you never created is the same. Build the thing, THEN
   document it.
3. **Never write a `docker-compose.yml` / `Makefile` / build script
   referencing a directory or file that doesn't exist.** Build
   artifacts that won't run on a fresh checkout are vapor and
   actively confusing — the operator runs `docker compose up`,
   gets an "no such directory" error, and now they don't trust
   anything else you said either.
4. **If your reply says "done" or "complete", every artifact you
   named must be on disk and pass `ls`.** Specifically: if you
   say "I built the backend at `/home/.../backend/`", that
   directory must exist with real code in it. If you say "I
   committed the changes", `git log` must show the commit.

## Knowing when to stop

- Match the change to what was asked. No drive-by refactors.
- Don't half-finish. If you can't complete in one pass, stop and
  say what's left.

## Related skills

- [[git-commit]], [[code-review]], [[testing]], [[todo-tracker]],
  [[agent-memory]]
