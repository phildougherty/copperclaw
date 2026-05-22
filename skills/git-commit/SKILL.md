---
name: git-commit
description: Discipline for using git from inside the container — when to stage, how to write a commit message, what to never do. Built on top of `shell` and the existing read-only `git_*` tools; opt-in for coding agents.
---

# git-commit

How to make a git commit safely. Read-only git inspection (status, log,
diff, blame) is always available via the dedicated `git_*` tools — use
those. Mutating operations (`git commit`, `git add`, etc.) happen
through `shell`. This skill is the discipline that wraps both.

## Always do first

**Inspect before you stage.** Run `git_status` and `git_diff` to see
what's changed. If there's anything unexpected, stop — investigate
unfamiliar files or branches before staging them.

**Stage specific files, not `-A` / `.`.** Naming files keeps you from
accidentally committing secrets (`.env`, credentials.json), build
artefacts, or someone else's in-flight work.

**Match the project's commit-message style.** Run `git_log` first to
see the recent shape. If the project writes "fix: …" Conventional
Commits, do that. If it writes terse imperative subjects, do that.

## Writing the message

**One short subject line, then a blank line, then the body.** The
subject describes the *why*, not the *what* — the diff shows the
what. Aim for under 70 characters in the subject.

**Be specific.** "Fix login bug" is not a useful subject. "Reject empty
password before bcrypt hash" is.

**Group the changes accurately.** "add" means a new feature. "fix"
means a bug fix. "refactor" means same behaviour, different code.
"change" means a meaningful behavioural change that isn't quite a new
feature. Don't mix these.

## Never do

- **Never amend a commit that's already been pushed.** Amend rewrites
  history; if anyone has pulled the previous commit, you've broken
  their tree. New commits over amend.
- **Never `git commit --no-verify` or `--no-gpg-sign` unless explicitly
  asked.** The pre-commit hooks exist for a reason. If a hook fails,
  fix the underlying issue and make a fresh commit.
- **Never `git push --force` without explicit permission.** Same
  reasoning as amend. If you have to overwrite a remote branch,
  confirm first.
- **Never `git reset --hard` or `git checkout --` on a tree with
  uncommitted work.** That's unrecoverable. If you need a clean slate,
  stash first.
- **Never `git clean -f` without showing the user what it would
  delete.** Untracked files are often the user's in-progress work.

## When the hook fails

When `pre-commit` (or any other hook) rejects the commit:

1. Read the hook's actual output. Don't just retry.
2. The commit did **not** happen — so `--amend` would modify the
   *previous* commit, not the failed one. Make a fresh commit after
   the fix.
3. If the hook fix is small (formatting, a typo), do it and re-commit.
4. If the hook fix is large or unclear, stop and ask.

## Composing a commit message

Use a heredoc to preserve formatting:

```bash
git commit -m "$(cat <<'EOF'
Subject line under 70 chars

Body explaining the why. Reference issue numbers if the project does
that. Multiple paragraphs are fine.
EOF
)"
```

Quoting matters: double-quoted heredocs expand variables, single-quoted
ones don't. Use single quotes around `EOF` unless you actually want
expansion.

## Related skills

- [[coding-task]] — what to do *before* the commit (tests, lint).
- [[code-review]] — checking a diff before deciding it's ready to ship.
