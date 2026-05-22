---
name: git
description: Inspect a git repository — branch state, commit history, diffs, and per-line blame — via the structured `git_status`, `git_log`, `git_diff`, and `git_blame` tools. Invoke when the user asks "what changed", "who wrote this", "is the working tree clean", "show me the last few commits", or anything that previously would have meant shelling out to `git`.
---

# git

Four read-only tools backed by libgit2. They return structured JSON,
not text the model has to parse. Use them in preference to
`shell git ...` — the output is stable, the errors are friendly, and
there's no pager to fight.

All four accept an optional `path` argument. Default is the current
working directory; the tool walks upward to find the repo. You can
also pass any path *inside* a repo (a file, a subdirectory) and the
tool will resolve to the enclosing `.git`.

These tools do not commit, push, branch, or modify anything. If the
user wants a mutation — hand the exact `git ...` command to the
operator and stop. Don't try to `shell` your way around it.

## When to use each

### `git_status`

> "Is the working tree clean?" / "what's modified?" / "did I leave
> something uncommitted?"

Returns the branch name, ahead/behind counts vs upstream, and the
list of staged / unstaged / untracked files. Cheap; call it first
before doing anything that depends on a clean state.

### `git_log`

> "What changed in the last hour?" / "show me the last 10 commits"
> / "who's been touching `src/auth/`?"

Walks commits reachable from a ref (default `HEAD`) and returns one
JSON object per commit with sha / short_sha / author / email /
RFC3339 date / subject / body / files_changed. Supports:

- `max_count` (default 20, cap 200).
- `since` filter (ISO date `2026-05-01` or full RFC 3339).
- `files` filter — restricts the result to commits that touch any
  of the listed paths.

### `git_diff`

> "What's the diff between `HEAD~1` and `HEAD`?" / "what
> uncommitted changes do I have?" / "show me what changed in
> `src/foo.rs` last commit"

Unified diff plus a per-file additions/deletions summary.

- Omit both `from` and `to` for the working-tree diff (equivalent
  to plain `git diff`).
- Set `from` and `to` for ref-to-ref diffs.
- `files` narrows to a pathspec.
- `context` controls unified-diff context lines (default 3).
- `max_bytes` caps the patch text (default 200 KiB, hard cap 1
  MiB). Truncated responses set `truncated: true` so you know to
  narrow the scope.

### `git_blame`

> "Who wrote this function?" / "when was line 42 last touched?"

Per-line blame for a file. Each row carries the short SHA, author
name, RFC 3339 date, and the line text. Range with `from_line` /
`to_line` (defaults 1 → end-of-file). Out-of-range bounds are
clamped to the file's actual size; inverted ranges return empty.

## Common patterns

- **"What did I just change?"** → `git_status` first (see if it's
  unstaged or staged), then `git_diff` with no `from`/`to` for the
  working-tree patch.
- **"What changed in the last hour?"** → `git_log` with
  `since: "<one-hour-ago RFC3339>"`. For each commit you care
  about, follow up with `git_diff { from: <sha>~1, to: <sha> }`.
- **"Why is this line here?"** → `git_blame` for the offending
  line range. The blame row's SHA is your jumping-off point for a
  `git_log` (filter by `files: ["<that-file>"]`) to see the wider
  context.
- **"Is it safe to edit?"** → `git_status` — if `clean: false`,
  warn the user before you do anything that could conflict with
  their uncommitted work.

## Triggers

- "git status" / "what's the state of the repo"
- "git log" / "recent commits" / "what changed lately"
- "git diff" / "show me the diff" / "what changed in `<ref>`"
- "git blame" / "who wrote this" / "when was this added"
- "is the working tree clean"
- Anything where the user previously would have asked you to
  `shell git ...` — reach for these first.

## Do NOT

- **Do not commit, push, branch, tag, reset, stash, or check out
  via these tools.** They're read-only by design. Hand mutations
  back to the operator with the exact `git` command.
- **Do not** parse text output from `shell git ...` if one of
  these tools could answer the question. Structured JSON is more
  reliable.
- **Do not** call `git_blame` on a huge file without narrowing
  the line range — it will return a validation error past 5,000
  lines. Use `from_line` / `to_line`.
- **Do not** treat `truncated: true` from `git_diff` as the full
  answer. Either raise `max_bytes` (up to 1 MiB) or narrow the
  scope with `files` / a tighter `from`..`to`.
- **Do not** assume HEAD exists. A freshly `git init`-ed repo has
  no commits — `git_log` returns `{"commits": []}` and
  `git_status` reports `branch: "(unborn)"`. Treat both as
  successful, not as errors.
