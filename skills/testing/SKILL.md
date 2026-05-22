---
name: testing
description: How to find the test suite, interpret failures, decide when to add tests. Opt-in for coding agents.
---

# testing

How to use tests effectively. The goal is to verify the change does
what it should, not to maximise test count.

## Finding the suite

Look for the language/framework's standard runner before reaching for
anything custom:

- Rust: `cargo test --workspace --no-fail-fast`. The project may have a
  faster scope (`cargo test -p <crate>`); use it when you know which
  crate you touched.
- Python: `pytest` (or `python -m pytest`) at the project root. Some
  projects use `tox` or `nox` as a wrapper — check `pyproject.toml`
  or the README.
- TypeScript / JavaScript: `npm test`, `pnpm test`, or `yarn test`
  depending on the lock file present.
- Go: `go test ./...`.

When in doubt, `read_file` the project's CONTRIBUTING.md or look for a
`scripts/test` / `Makefile` / `justfile` entry — the project will
usually document its canonical command.

## Interpreting failures

**Read the actual failure message before deciding what's wrong.** A
test that fails with "assertion left == right" but with cleanly
matching values is not the same as one that fails with "panicked at
…unwrap()".

**Distinguish 'flake' from 'broken'.** A test that fails because of a
race, a network call, or a timing assumption is a flake — re-run it
once before assuming you broke it. A test that fails the same way
every time is a real regression.

**Read the test that failed.** Don't guess what it's checking from
the name. The test body tells you what behaviour the project
considers correct.

**Find the most recent passing commit.** When a previously-green test
goes red after your change, `git bisect` (or just reading the diff)
is faster than staring.

## Adding tests

**Add tests when the change introduces new behaviour without
coverage.** Bug fixes deserve a regression test that fails before the
fix and passes after.

**Don't add tests for behaviour the existing suite already covers.**
Duplicate tests are noise — when they break, they break together,
and they slow the suite down.

**Test names should describe the behaviour, not the function.**
"add_two_returns_three_when_inputs_are_one_and_two" beats
"test_add_two". The name is the first thing someone reads when the
test fails.

**Test the interface, not the implementation.** Tests that lock the
implementation down break on every refactor, even when the behaviour
is unchanged.

## Don't

- **Don't disable a failing test to make the suite green.** That's
  hiding a regression. Either fix the test or fix the code; if you
  can't, stop and ask.
- **Don't mock things the production code wouldn't call.** Mocks
  should match how the code actually invokes the dependency, not
  some idealised version.
- **Don't add a test that doesn't actually exercise the new code.**
  Re-read the test body and make sure the new line(s) you added are
  reachable from the test.

## When the suite is too slow to run on every change

Some projects have multi-minute (or multi-hour) test suites. When you
can, run a narrower scope first (`cargo test -p <crate>` /
`pytest tests/unit/`) and only fall back to the full suite when the
narrow scope passes. Don't claim a change is done without at least
the narrow scope green.

## Related skills

- [[coding-task]] — when a test is part of "done" and when it isn't.
- [[git-commit]] — don't commit until the relevant tests pass.
