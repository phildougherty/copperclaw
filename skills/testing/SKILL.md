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

## Iterating to green

Fixing a failing test is a loop, not one shot:

1. Run the test, read the **actual** failure — never patch blind off
   the test name.
2. Make the smallest change that addresses *that* failure.
3. Re-run the same narrow scope. Repeat.

- **One hypothesis per iteration.** Change three things at once and a
  green result tells you nothing about which one worked.
- **Cap your attempts.** Same test still red after a few focused
  tries: stop guessing. Re-read the test and the code under test
  together, or say "stuck on X, here's what I ruled out." Ten
  near-identical patches usually means you're fixing the wrong thing.

Don't widen to the full suite until the narrow scope is green; then
run it once to catch anything you knocked over elsewhere.

## The verify contract (`.copperclaw/verify`)

Copperclaw *enforces* verification for coding agents — the check you run
is the check the runner watches. On your first edit inside a
`/data/<project>`, record the project's one-line check command in
`/data/<project>/.copperclaw/verify` (`npm test`, `cargo check`,
`python -m pytest`, `go test ./...`, or a smoke `curl` for a server). A
per-group `check_command` config can override it.

After any edit the project is **dirty** until that exact command runs
green, and `todo_update(status="completed")` is refused while dirty (the
error names the project and the recorded command). To clear it, run the
recorded command via `shell` with `cwd` set to the project dir — exit 0
clears dirty; a nonzero exit records a fix cycle. Two failing cycles
auto-`blocked` the todo. So the command you put in `.copperclaw/verify`
is the one that has to pass: make it the real suite, not a stub that
always exits 0.

## Reading the end of a long test log

A failing suite prints its error at the END, but `shell` keeps the FIRST
32 KiB of output by default — the tail gets cut off. Re-run keeping the
tail instead with `tail_bytes`: `{command:"npm test", tail_bytes:16384}`.
For a log already on disk, page it with `read_file` (`mode:"lines"`; the
result reports `total_lines`, so advance `offset` window by window) or
`shell tail -n 200 <path>`.

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
