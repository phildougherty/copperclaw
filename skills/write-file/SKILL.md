---
name: write-file
description: Write UTF-8 text to a file inside the container with the write_file tool, including auto-mkdir and append semantics.
---

# write-file

`write_file` writes a string to a file inside the session container.
By default it creates the parent directories and overwrites the
destination; pass `append: true` to grow an existing file instead.

## Schema

```json
{
  "path": "string, non-empty",
  "content": "string",
  "create_parents": "boolean (default true)",
  "append": "boolean (default false)"
}
```

- `path` (required). Absolute or relative. Relative paths resolve
  against the runner's working directory.
- `content` (required). UTF-8 text. For binary output, base64-encode
  with `shell` instead.
- `create_parents` (optional, default `true`). When `true`, missing
  parent directories are created (`mkdir -p` semantics). When
  `false`, a missing parent yields an error.
- `append` (optional, default `false`). When `true`, the file is
  opened with `O_APPEND | O_CREAT`. When `false` (default), the
  file is truncated then written.

## Result shape

```json
{
  "path": "/data/workspace/notes.txt",
  "bytes_written": 142,
  "appended": false
}
```

## Behaviour matrix

| append | file exists | result |
|---|---|---|
| `false` (default) | yes | overwritten |
| `false` | no | created |
| `true` | yes | appended (file kept) |
| `true` | no | created, then appended (same as overwrite of empty file) |

## Diff card surfaced to the user

When `write_file` overwrites an existing file the host emits a
structured diff card to the originating channel showing what changed.
Pure first-writes and `append: true` calls skip the card (no "before"
to diff against). You don't need to summarise the change in prose;
the user already sees it. Files over 256 KB skip the full diff and
get a one-line `before / after` size summary instead.

## When to prefer other tools

- **Editing a few bytes in the middle of a large file**: use `shell`
  with `sed -i` or a similar in-place tool. `write_file` writes the
  whole content; you would have to read-modify-rewrite manually.
- **Writing binary data**: base64-decode via `shell` and write with
  `dd` / `tee`, or use `shell` directly with a here-doc.
- **Writing files outside `/data`**: legal but lost on next spawn.
  The container's root filesystem is ephemeral; only `/data`
  persists across runner restarts.

## Caveats

- The write is **not** atomic at the filesystem level. If you need
  an atomic replacement, write to `<path>.tmp` then call `shell`
  with `mv <path>.tmp <path>`.
- The tool does not chmod the result; default umask applies.
- `path` containing `..` segments is resolved by the OS as normal —
  there is no sandboxing inside the container, so a path like
  `../etc/passwd` will write where it claims.
