---
name: read-file
description: Read a UTF-8 file from the container filesystem with the read_file tool, including the 128 KiB cap and offset/limit/mode paging for reading large files window by window.
---

# read-file

`read_file` returns the UTF-8 contents of a file inside the session
container. It is the right tool for reading text files; for binary
data, archives, or large blobs use `shell` and pipe through the
appropriate decoder.

## Schema

```json
{
  "path": "string, non-empty",
  "offset": "integer >= 0 (optional)",
  "limit": "integer >= 0 (optional)",
  "mode": "\"bytes\" (default) or \"lines\" (optional)"
}
```

- `path` (required). Absolute or relative path. Resolved by the
  runner's process — relative paths use the runner's working
  directory.
- `mode` (optional). `bytes` (default): `offset` is a 0-indexed
  byte position, `limit` a byte count. `lines`: `offset` is a
  1-indexed line number, `limit` a line count.
- `offset` / `limit` (optional). Read window. Negative values are
  rejected — for tail reads use `shell` with `tail -n N`.

## Output limits

The tool returns at most 128 KiB per call, regardless of `limit`.
Past the cap the read stops and `truncated: true` is set, along
with `bytes_read` so you can tell how much you got. Out-of-range
offsets return an empty body with `truncated: false` — a clean
end-of-file signal, not an error.

To page through a large file (build log, dataset), prefer `lines`
mode: start at `{ "mode": "lines", "offset": 1, "limit": 200 }`,
then advance `offset` (201, 401, ...) — the result includes
`total_lines` for the whole file so you know exactly how many
windows remain. For an 8 KiB slice at a byte position, use
`{ "mode": "bytes", "offset": 1000000, "limit": 8000 }`.

## Result shape

```json
{
  "path": "/data/notes.txt",
  "body": "...",
  "truncated": false,
  "bytes_read": 14021,
  "offset": 0,
  "limit_applied": 131072,
  "mode": "bytes",
  "total_lines": 342
}
```

`total_lines` is present in `lines` mode only.

## Errors

- Non-UTF-8 bytes are returned via `String::from_utf8_lossy` — invalid
  sequences appear as the U+FFFD replacement character. For exact bytes,
  decode explicitly with `shell` (`iconv`, `base64`, etc.).
- Missing path returns `Internal`. Check with
  `shell { "command": "test -f <path>" }` first if the file's
  existence is uncertain.
- Directories return an error — use `shell { "command": "ls -la <dir>" }`
  for listings.

## When to prefer other tools

- **Listing a directory**: `shell` with `ls`.
- **Finding files by name pattern**: `glob` — see [[glob]].
- **Searching file contents**: `grep` — see [[grep]]. Don't read a
  whole file just to find one string.
- **Reading a file the agent itself wrote earlier in the turn**:
  prefer to keep the data in conversational memory; the round-trip
  is wasted.
- **Reading inbound attachments**: the runner already lowers
  attachments into `/data/inbox/<msg_id>/`. The path is in the
  inbound message's `content.attachment.bytes_path` field.
- **Modifying what you just read**: [[edit-file]] to change a
  region in place, [[write-file]] to replace the whole file.
