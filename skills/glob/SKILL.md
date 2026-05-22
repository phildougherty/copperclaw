---
name: glob
description: List files under a path matching a gitignore-style glob with the glob tool. Returns sorted paths suitable for feeding into read_file or grep.
---

# glob

`glob` returns the list of files under a directory whose paths match
a gitignore-style glob pattern (`**/*.rs`, `crates/**/Cargo.toml`,
`*.md`). Results are sorted lexicographically so the output is
stable across calls.

Use this when you know the *kind* of file you want but not the exact
path. The typical pattern is `glob` to enumerate candidates, then
`read_file` or `grep` on each. Prefer it over
`shell { command: "find ..." }` — the structured list is faster,
cheaper, and skips the parsing.

## Schema

```json
{
  "pattern": "string, non-empty, gitignore-style glob",
  "path": "string (optional, defaults to cwd)",
  "max_results": "integer (optional, default 1000, cap 10000)",
  "no_ignore": "bool (optional, default false)"
}
```

- `pattern` (required). Standard glob syntax: `*`, `?`, `**`,
  character classes (`[abc]`). The matcher tries the bare
  filename AND the path relative to the search root, so both
  `*.rs` and `crates/**/*.rs` work without surprises.
- `path` (optional). Search root. Defaults to the runner's
  cwd. Absolute paths produce absolute results; relative roots
  produce workspace-relative results.
- `max_results` (optional). Hard ceiling 10000. Above that the
  agent should narrow the pattern.
- `no_ignore` (optional). Bypass `.gitignore` / `.ignore`. The
  unconditional skip list (`target/`, `node_modules/`, `.git/`)
  still applies — there is no flag to traverse those.

## Result shape

```json
{
  "paths": ["crates/bar/src/main.rs", "crates/foo/src/lib.rs"],
  "truncated": false,
  "total_matched": 247
}
```

- `paths` is sorted ascending.
- `truncated: true` means more files matched than the cap allowed.
  `total_matched` reports how many we counted before stopping.

## When to prefer other tools

- **Searching file contents**: use `grep`. `glob` does NOT open
  any file; it only matches names.
- **Reading a single known path**: use `read_file` directly.
- **Listing only the immediate directory** (`ls`): use
  `shell { command: "ls" }` — `glob` is recursive by design and
  there's no flag to disable that.

## Errors

- Invalid glob (`[invalid`) returns `Validation` with the pattern
  named in the message.
- Non-existent `path` returns `Validation`.
- A pattern with no matches returns an empty `paths` array with
  `total_matched: 0` — never an error.

## Examples

Find every Rust source file in the workspace:

```json
{ "pattern": "**/*.rs" }
```

Find all migration files under a specific crate:

```json
{
  "pattern": "**/*.sql",
  "path": "crates/ironclaw-db/migrations"
}
```

Find `Cargo.toml` files in any subcrate of `crates/`:

```json
{ "pattern": "crates/*/Cargo.toml" }
```

Find files in build output (`.gitignore`d normally):

```json
{
  "pattern": "**/*.so",
  "path": "/build",
  "no_ignore": true
}
```
