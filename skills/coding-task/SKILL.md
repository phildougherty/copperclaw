---
name: coding-task
description: Disciplines for doing real coding work — editing files, running tests, verifying that what you built actually works, and delivering the artifacts so the operator can use them. Opt-in via `SkillsSelector::Explicit` on the agent group; non-coding agents should not enable this.
---

# coding-task

How to do coding work as an Ironclaw agent. The base session image
ships with `python3`, `pip`, `node`, `npm`, `git`, `curl`, `wget`,
`jq`, and `build-essential` available via `shell`. Use them.

## Doing the task

- **Read before you write.** Open files with `read_file` first.
  Match the surrounding style.
- **Prefer editing existing files.** Don't create a new file unless
  the task needs one.
- **Don't add features the user didn't ask for.** No surrounding
  cleanup, no speculative helpers, no future-proofing.
- **Don't add error handling for impossible cases.** Trust internal
  guarantees. Validate at system boundaries only.
- **Comments only when the *why* is non-obvious.** Restating the
  code is noise.

## Verifying the change works (NOT OPTIONAL)

**You do not claim "done" until you ran the code.** "Production-
ready", "complete", "working" — those are claims about evidence,
not vibes. If you have no evidence, you don't have the claim.

Concrete verification recipes:

- **Python script** — `python3 path/to/file.py` (or with a sample
  input). Confirm exit 0 and the expected output.
- **Python module** — `python3 -c 'import path.to.module; print("ok")'`
  to catch import-time errors at minimum.
- **Node app** — `node path/to/file.js`. For an Express/HTTP server,
  also: spawn it on a port, `curl http://localhost:<port>/`, and
  confirm a real response before declaring it works.
- **HTML page** — `head` the file to confirm structure;
  `python3 -m http.server` in the dir and `curl` your test page
  to verify it serves; visually you can't preview from the
  container, but at least confirm the file is syntactically valid
  HTML and the assets it references exist.
- **Any script** — if there's a test command (`pytest`, `npm test`),
  run it. The bar is higher than "it compiles."

If the verification fails or you can't run it, **say so in your
reply**. "I wrote X but couldn't run it because Y" is honest.
"Production-ready" without evidence is not.

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
results." Never invent "238K user complaints reviewed" or "$12M ARR
projection" or research stats you didn't actually compute. If the
data isn't real, don't pretend it is.

## Knowing when to stop

- **Match the change to what was asked.** No drive-by refactors.
- **Don't half-finish.** If you can't complete in one pass, stop
  and ask.
- **Stop after the substantive change.** No "here's what I built"
  prose restating the diff.

## Related skills

- [[git-commit]], [[code-review]], [[testing]], [[todo-tracker]],
  [[agent-memory]]
