---
name: edit-file
description: Modify an existing file by replacing a unique substring with the edit_file tool. Cheaper and safer than rewriting the whole file with write_file when you only need to change a few lines.
---

# edit-file

`edit_file` swaps an exact substring inside an existing file. Always
prefer it over `write_file` when modifying an existing file — you
emit just the diff instead of re-typing the whole file, which is
cheaper in tokens and impossible to corrupt with stray whitespace
changes elsewhere.

## Schema

```json
{
  "path": "string, non-empty",
  "old_string": "string, non-empty",
  "new_string": "string",
  "replace_all": "boolean (default false)"
}
```

- `path` (required). Path to an existing regular file. Symlinks are
  followed; the target must also be a regular file.
- `old_string` (required). Literal substring to replace. **Must
  appear exactly once** in the file unless `replace_all` is true —
  the uniqueness check is what makes the tool safe.
- `new_string` (required). Replacement substring. Must differ from
  `old_string` (no-op edits are rejected).
- `replace_all` (optional, default `false`). Replace every
  occurrence. Use for renames / refactors where you do want to hit
  every site.

## Result shape

```json
{
  "path": "/data/workspace/main.rs",
  "replacements": 1,
  "bytes_written": 1842
}
```

## How to use it

1. **`read_file` first.** Pull the file (or the relevant region via
   `shell` with `grep -n` / `sed -n`) so you can see the exact byte
   sequence you want to swap.
2. **Include enough surrounding context to disambiguate.** If the
   line you want to change appears elsewhere in the file, expand
   `old_string` to include the function header, a unique comment,
   or the line before / after — whatever it takes to make the
   match unique. The tool will reject ambiguous matches with a
   message telling you how many times it saw your string, so the
   "include more context" iteration is short.
3. **For renames, use `replace_all: true`.** Renaming a symbol,
   shifting a path, bumping a version string — anything where you
   genuinely want every occurrence rewritten.
4. **For brand-new files, use `write_file` instead.** `edit_file`
   refuses to create files (that's `write_file`'s job).

## Errors you should expect

- `not found` — `old_string` doesn't match anywhere. Re-`read_file`
  the relevant region and copy the exact bytes.
- `matches N times` — ambiguous. Add surrounding context until the
  match is unique, or set `replace_all` if you wanted them all.
- `must differ` — `old_string` and `new_string` were the same.
  Probably a paste error.
- `not a regular file` — path is a directory or a symlink to a
  non-file (e.g. a socket).
- `not found` at the path level — file doesn't exist. Use
  `write_file` to create it.

## Atomicity guarantees

The write goes through a sibling temp file in the same directory,
`fsync`s, then `rename(2)`. A crash mid-write leaves the original
untouched. The file's mode (permissions) survives the swap; owner
does not necessarily (chown needs caps the container doesn't have).

## When to prefer other tools

- **Creating a new file:** `write_file`.
- **Appending without changing existing content:** `write_file`
  with `append: true`.
- **Multi-line in-place edit driven by a regex:** `shell` with
  `sed -i -E '...'`. `edit_file` is literal-string-only by design.
- **Bulk find-and-replace across many files:** loop `edit_file`
  calls, one per file. The tool is one-file-per-call.
