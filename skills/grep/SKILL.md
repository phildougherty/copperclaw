---
name: grep
description: Regex-search files under a path with the grep tool. Returns structured {path, line, text} rows so you can act on the matches without parsing shell output.
---

# grep

`grep` is a native regex search across the container filesystem. It
walks the directory tree honouring `.gitignore` (the same `ignore`
crate `ripgrep` uses), skips binary files, and returns one structured
row per hit: `{path, line, text, context_before, context_after}`.

Prefer this over `shell { command: "grep -rn ..." }` whenever the
match itself matters — the structured form is faster, cheaper in
tokens, and immune to shell-escaping bugs around weird filenames or
patterns containing spaces.

## Schema

```json
{
  "pattern": "string, non-empty, regex",
  "path": "string (optional, defaults to cwd)",
  "glob": "string (optional, *.rs / **/*.toml filter)",
  "case_insensitive": "bool (optional, default false)",
  "max_results": "integer (optional, default 100, cap 1000)",
  "context_lines": "integer (optional, default 0, cap 20)",
  "no_ignore": "bool (optional, default false)"
}
```

- `pattern` (required). Standard Rust `regex` syntax — no PCRE
  back-references. For OR, prefer `\b(foo|bar)\b` in one call
  rather than two separate calls.
- `path` (optional). Search root. Relative paths resolve against
  the runner's cwd; absolute paths land where you'd expect.
- `glob` (optional). Filename / path filter. Both `*.rs` and
  `crates/**/src/lib.rs` work — the matcher tries the bare
  filename and the path-relative-to-root.
- `case_insensitive` (optional). Default false.
- `max_results` (optional). Hard ceiling 1000. Above that the
  agent should narrow the pattern or glob.
- `context_lines` (optional). Lines of context around each hit.
  Capped at 20.
- `no_ignore` (optional). Bypass `.gitignore` / `.ignore` when
  you really need to search ignored content (logs, generated
  files). `target/`, `node_modules/`, and `.git/` are still
  skipped unconditionally — there is no flag to disable that.

## Result shape

```json
{
  "matches": [
    {
      "path":           "crates/foo/src/lib.rs",
      "line":           42,
      "text":           "fn run_loop() {",
      "context_before": ["", "/// Drive the runner."],
      "context_after":  ["    loop {", "        ..."]
    }
  ],
  "truncated":     false,
  "total_matched": 17
}
```

- `path` is workspace-relative when the search root was relative,
  absolute otherwise.
- `text` is truncated to 4 KiB on a UTF-8 boundary with a
  `…[truncated]` marker.
- `truncated: true` means there were more matches than
  `max_results` allowed — narrow the search or raise the cap.

## When to prefer other tools

- **Listing files by pattern without searching contents**: use
  `glob`. It's the right tool when you only need the path list.
- **Reading a single known file**: use `read_file`. Don't grep
  for "everything in this file".
- **Counting occurrences**: `grep` then count `matches.len()` on
  the caller side. Don't spawn `shell wc -l`.
- **Running an actual `grep` binary with flags this tool doesn't
  expose** (`-A`/`-B` asymmetric context, perl-compatible regex):
  fall back to `shell` then.

## When text search isn't enough

`grep` matches *text*. Two jobs it does badly, and what beats it:

- **"Find every caller / every implementor" before a refactor.** A
  regex over a name catches comments, strings, and same-named symbols
  in unrelated scopes, and misses calls split across lines. The
  precise oracle is the language's own checker: change the signature
  and run `cargo check` / `tsc --noEmit` / `go build` / `mypy` — every
  real reference surfaces as an error, zero false positives. Safest
  way to scope a refactor.
- **Structural matches** ("calls to `foo(` with a literal first arg",
  "every `match` with no wildcard arm"). Text regex can't see syntax;
  `ast-grep` matches on the parse tree across most languages (e.g.
  `ast-grep -p 'foo($A)'`). Not in the base image — `npm i -g
  @ast-grep/cli` when a refactor actually needs it.

Reach for these when correctness matters; for everyday "where is this
string" lookups, plain `grep` is right.

## Errors

- Invalid regex (`(invalid`) returns `Validation` with the
  pattern named in the message.
- Non-existent `path` returns `Validation`.
- Permission / IO errors on individual files are logged and
  skipped — they never fail the whole search.

## Examples

Find every place a symbol is defined or referenced in the
workspace:

```json
{ "pattern": "fn\\s+run_loop\\b", "glob": "**/*.rs" }
```

Find a TODO marker, case-insensitive, with two lines of context:

```json
{
  "pattern": "TODO|FIXME",
  "case_insensitive": true,
  "context_lines": 2,
  "glob": "**/*.{rs,toml,md}"
}
```

Search log files that are normally `.gitignore`d:

```json
{
  "pattern": "ERROR.*timeout",
  "path": "/var/log",
  "no_ignore": true
}
```
