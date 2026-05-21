---
name: read-file
description: Read a UTF-8 file from the container filesystem with the read_file tool, including the 1 MiB cap and char-boundary-safe truncation semantics.
---

# read-file

`read_file` returns the UTF-8 contents of a file inside the session
container. It is the right tool for reading text files; for binary
data, archives, or large blobs use `shell` and pipe through the
appropriate decoder.

## Schema

```json
{ "path": "string, non-empty" }
```

- `path` (required). Absolute or relative path. Resolved by the
  runner's process — relative paths use the runner's working
  directory.

## Output limits

The tool returns at most 1 MiB of content. Files larger than that
are truncated at a UTF-8 character boundary and a `truncated: true`
flag is set in the result, along with `bytes_read` and `total_bytes`
so you can tell how much you got.

If you need a tail or a specific section of a large file, use
`shell` with `tail`, `head`, `sed -n`, or `awk`:

```json
{ "command": "tail -200 /var/log/long.log" }
```

## Result shape

```json
{
  "path": "/data/notes.txt",
  "content": "...",
  "bytes_read": 14021,
  "total_bytes": 14021,
  "truncated": false
}
```

## Errors

- Non-UTF-8 bytes return a validation error. Decode explicitly with
  `shell` (`iconv`, `base64`, etc.).
- Missing path returns `Internal`. Check with
  `shell { "command": "test -f <path>" }` first if the file's
  existence is uncertain.
- Directories return an error — use `shell { "command": "ls -la <dir>" }`
  for listings.

## When to prefer other tools

- **Listing a directory**: `shell` with `ls`.
- **Reading a file the agent itself wrote earlier in the turn**:
  prefer to keep the data in conversational memory; the round-trip
  is wasted.
- **Reading inbound attachments**: the runner already lowers
  attachments into `/data/inbox/<msg_id>/`. The path is in the
  inbound message's `content.attachment.bytes_path` field.
