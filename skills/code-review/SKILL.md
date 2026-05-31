---
name: code-review
description: How to read a diff and produce a focused review — what to look for, what to ignore, how to summarise. Built on the existing `git_diff` tool. Opt-in for coding agents.
---

# code-review

How to review a diff someone else (or you, earlier) wrote. The goal is
a short, actionable summary — not a re-implementation of the change.

## Read the diff before anything else

Use `git_diff` (or `shell` with `git diff <base>...HEAD` for branch-
relative diffs). Don't ask the user to paste the diff; reach for the
tool.

Skim once to get the shape of the change. Note which files are
touched, the rough sizes, and whether the change is one logical
operation or several mixed together.

## What to look for, in order

1. **Does the change do what the description says?** A diff that's
   bigger than its claim is a red flag — either the description is
   wrong or there's unrelated work mixed in.
2. **Correctness bugs.** Off-by-one, null checks missed, error paths
   that swallow the error, race conditions in concurrent code,
   resource leaks. These are the bugs that cost real time later.
3. **Security.** SQL injection, command injection, XSS, path
   traversal, secret leakage. Treat any new string-interpolated SQL,
   shell, or HTML as a yellow flag worth a second look.
4. **Test coverage.** New behaviour without a test is suspicious. A
   test that doesn't actually exercise the new code is worse than no
   test (false confidence).
5. **API surface changes.** New public functions, renamed public
   functions, changed argument shapes — these affect callers the diff
   doesn't show.
6. **Performance, only when relevant.** A new N² loop in a hot path
   matters. A new N² loop in a one-time CLI command does not.

## What to ignore

- **Style nits the linter already covers.** If the project runs a
  formatter, don't fight the formatter in review.
- **Personal preferences for "this could be cleaner".** Cleanliness
  isn't free — every refactor is a future merge conflict for someone.
  Comment on cleanliness only when the current state is *actively*
  harder to read, not when you'd prefer something different.
- **Code that the diff didn't touch.** "Could we also fix X while
  we're here" is scope creep — surface it as a separate ticket.

## Adversarial pass — try to break it

Reading for correctness catches the bugs you can see; the ones that
bite later are the inputs the author didn't picture. Before signing
off on anything non-trivial, spend sixty seconds attacking it:

- **Boundaries:** empty, single element, huge, zero, negative,
  off-by-one at the ends of every loop and slice.
- **Malformed input:** wrong types, missing fields, oversized
  payloads, embedded quotes/newlines, non-UTF-8.
- **Concurrency:** runs twice, or interleaves with another writer — is
  the "check then act" actually atomic?
- **Error paths:** force the failure (network down, file missing,
  parse error) and confirm it's handled, not swallowed.

For most changes that's an in-context pass — you, thinking like an
attacker. For genuinely high-stakes code (auth, money, data loss,
anything hard to roll back), spin up a dedicated critic with
`create_agent`: hand it the diff and one instruction — "find inputs
that break this and prove it with a repro." A second context with an
adversarial mandate sees what the author's context is blind to. It
costs a full sibling agent, so reserve it for changes that earn it.

## Producing the summary

Reply with at most a few short sections:

- **Verdict.** "Looks good to merge," "needs changes before merging,"
  or "I have questions before I can decide."
- **Findings.** Concrete issues with file paths and line numbers. One
  per bullet. State what's wrong, not what to do — the author often
  knows the right fix once you point at the problem.
- **Questions.** Anything you can't answer from the diff alone.

Don't:

- Restate the entire diff in prose.
- Praise verbosely. "LGTM" is enough when there's nothing to flag.
- Recommend rewriting code you didn't review. If you want a different
  approach, say so once and let the author decide.

## When the diff is too big

If the diff covers thousands of lines or spans many unrelated
subsystems, the right review feedback is: "this is too big to review
well; can it be split?" That's the most useful thing you can say in
that case — don't try to grind through it anyway.

## Related skills

- [[coding-task]] — author-side discipline for what should land in a
  diff in the first place.
- [[git-commit]] — what the commit message should look like once the
  change is ready.
- [[create-agent]] — spawn an adversarial critic for high-stakes
  changes (see the adversarial pass above).
- [[testing]] — the test discipline that backs the coverage check.
