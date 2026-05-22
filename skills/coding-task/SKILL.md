---
name: coding-task
description: Disciplines for doing real coding work — editing files, running tests, deciding when to add comments, when to stop. Opt-in via `SkillsSelector::Explicit` on the agent group; non-coding agents should not enable this.
---

# coding-task

How to do coding work as an Ironclaw agent. This skill captures the
behaviour the operator wants when an agent is asked to write or change
code — distinct from messaging, scheduling, or support agents who would
not have this skill enabled.

## Doing the task

**Read before you write.** Open the file you're about to change with
`read_file` first. Skim the surrounding code so the change matches the
style already there.

**Prefer editing existing files.** Don't create a new file unless the
task genuinely needs one. The repository's existing structure is almost
always the right place for new code.

**Don't add features the user didn't ask for.** A bug fix doesn't need
surrounding cleanup. A one-shot operation doesn't need a helper. Don't
design for hypothetical future requirements.

**Don't add error handling for impossible cases.** Trust internal code
and framework guarantees. Validate at system boundaries (user input,
external APIs) — not at every internal boundary.

**Don't add comments unless they pay rent.** Most comments restate what
the code already says. Write a comment only when the *why* is
non-obvious: a hidden constraint, a workaround for a specific bug,
behaviour that would surprise a reader. If removing the comment
wouldn't confuse a future reader, don't write it.

## Verifying the change works

**Run the tests before declaring done.** If the project has tests for
the area you touched, run them. If your change should have a test and
doesn't, add one — but stay focused; don't expand the test plan beyond
the change.

**Type-check and lint pass.** A change isn't done if `cargo check`,
`tsc`, `mypy`, or `clippy` complains about it. Fix the warnings, don't
suppress them, unless the suppression is the genuinely-right answer
(in which case write a brief comment explaining why).

**Don't claim a UI works without running it.** Type-checking and tests
verify correctness, not feature behaviour. If the change is visible to
a human, exercise it — start the dev server, click the thing, watch
for regressions. If you can't, say so explicitly rather than claiming
success.

## Knowing when to stop

**Match the change to what was asked.** A one-line fix gets a one-line
diff. Don't refactor adjacent code, rename variables, or "improve"
formatting because you were in the file.

**Don't half-finish.** If you can't complete the task in a single pass,
stop, summarise where you are, and ask. Leaving an in-flight refactor
in the tree is worse than not starting.

**Stop after the substantive change.** Don't write a summary paragraph
restating what you did — the diff already says that. Cross-reference
[[todo-tracker]] for step-tracking and [[agent-memory]] for things the
user wants you to remember next time.

## Related skills

- [[git-commit]] — once the work is done, how to stage and commit it.
- [[code-review]] — reading a diff someone else wrote.
- [[testing]] — finding the suite, interpreting failures.
- [[planning]] (via [[todo-tracker]]) — breaking multi-step work into
  trackable items.
