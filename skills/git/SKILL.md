---
name: git
description: Inspect a git repository — branch state, commit history, diffs, and per-line blame — via the structured `git_status`, `git_log`, `git_diff`, and `git_blame` tools. Invoke when the user asks "what changed", "who wrote this", "is the working tree clean", "show me the last few commits", or anything that previously meant shelling out to `git`.
---

# git

Four read-only tools backed by libgit2. Structured JSON, not text the
model has to parse. Prefer over `shell git ...` — stable output,
friendly errors, no pager.

All four take an optional `path` (default cwd). The tool walks upward
to the `.git`; any path *inside* a repo works.

These tools never commit, push, branch, or modify. For mutations: hand
the exact `git ...` command to the operator and stop. Do not `shell`
around it.

## When to use each

### `git_status`

> "Is the working tree clean?" / "what's modified?" / "did I leave
> something uncommitted?"

Branch name, ahead/behind vs upstream, staged / unstaged / untracked
lists. Cheap; call first before anything that needs a clean state.

### `git_log`

> "What changed in the last hour?" / "show the last 10 commits" /
> "who's touching `src/auth/`?"

Walks from a ref (default `HEAD`); one JSON object per commit with
sha / short_sha / author / email / RFC3339 date / subject / body /
files_changed.

- `max_count` (default 20, cap 200).
- `since` (ISO date `2026-05-01` or full RFC 3339).
- `files` — restrict to commits touching any listed path.

### `git_diff`

> "Diff between `HEAD~1` and `HEAD`?" / "what uncommitted changes
> do I have?" / "what changed in `src/foo.rs` last commit?"

Unified diff plus per-file additions/deletions summary.

- Omit both `from` and `to` → working-tree diff.
- Set both → ref-to-ref diff.
- `files` narrows by pathspec.
- `context` controls unified-diff context (default 3).
- `max_bytes` caps the patch (default 200 KiB, hard cap 1 MiB).
  Truncated responses set `truncated: true`.

### `git_blame`

> "Who wrote this function?" / "when was line 42 last touched?"

Per-line blame: short SHA, author, RFC 3339 date, line text.
`from_line` / `to_line` range (defaults 1 → EOF). OOB bounds clamp to
file size; inverted ranges return empty.

## Common patterns

- **"What did I just change?"** → `git_status` first, then `git_diff`
  with no `from`/`to`.
- **"What changed in the last hour?"** → `git_log { since: "<RFC3339>" }`,
  then `git_diff { from: <sha>~1, to: <sha> }` per commit.
- **"Why is this line here?"** → `git_blame` for the range; the
  blame SHA seeds a `git_log { files: ["<file>"] }`.
- **"Safe to edit?"** → `git_status`; if `clean: false`, warn the
  user before editing.

## Triggers

- "git status" / "state of the repo"
- "git log" / "recent commits" / "what changed lately"
- "git diff" / "show the diff" / "what changed in `<ref>`"
- "git blame" / "who wrote this" / "when was this added"
- "is the working tree clean"
- Anything the user would have framed as `shell git ...`.

## Do NOT

- Commit, push, branch, tag, reset, stash, or check out. Hand back
  the exact `git` command to the operator.
- Parse text from `shell git ...` when these tools answer it.
- `git_blame` a huge file without `from_line` / `to_line` — past
  5,000 lines returns a validation error.
- Treat `truncated: true` as the full answer — raise `max_bytes`
  (up to 1 MiB) or narrow with `files` / tighter `from`..`to`.
- Assume HEAD exists. Freshly `git init`-ed: `git_log` returns
  `{"commits": []}`; `git_status` reports `branch: "(unborn)"`.
  Both are success, not errors.
