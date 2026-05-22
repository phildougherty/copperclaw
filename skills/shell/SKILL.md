---
name: shell
description: Run a bash command inside the agent container with the shell tool — stdout, stderr, exit code, and an 8-byte output cap.
---

# shell

`shell` executes a bash command inside the session container and
returns the resulting stdout, stderr, and exit code. The container is
the isolation boundary: you're free to touch the filesystem, install
ad-hoc tools, hit local sockets, or run multi-step pipelines. Nothing
persists beyond the session's container lifetime.

## Schema

```json
{
  "command": "string, non-empty",
  "cwd": "string (optional)",
  "timeout_secs": "integer (optional, max 600)",
  "reset": "boolean (optional, default false)"
}
```

- `command` (required). Passed verbatim to `bash -c`. Shell features
  work — pipes, redirects, `&&`, subshells, here-docs.
- `cwd` (optional). Working directory. Defaults to whatever the
  runner started in (typically `/`).
- `timeout_secs` (optional). Soft cap on wall time. Default 60s,
  ceiling 600s. The model cannot disable timeouts.
- `reset` (optional). When true, wipe the persistent shell state
  file before running. Discards prior `cd` and exported env vars.

## Persistent state across calls

Within a single session the shell carries state forward between
calls. Specifically:

- The working directory: `cd /repo` in one call, then `pwd` in the
  next call prints `/repo`. No need to thread `cwd` through every
  call.
- Exported environment variables: `export FOO=bar` then `echo $FOO`
  in the next call prints `bar`.

State is per-session — different sessions never share, and nothing
survives container restart. To start clean mid-session pass
`reset: true` on the call you want to begin from.

Secret hygiene: variables whose name ends in `_TOKEN`, `_KEY`, or
`_SECRET`, or starts with `ANTHROPIC_`, are dropped from the
persisted snapshot. They're visible *within* the call that exported
them, but the next call sees them empty — re-export explicitly if
you need them across calls (and only if it's safe to do so).

## Output limits

stdout and stderr are independently truncated at 64 KiB each. A
trailing `\n…[truncated]` marker indicates a clipped stream. If you
expect a noisy command, pipe through `head`, `tail`, or `grep` before
the output reaches the tool boundary:

```json
{ "command": "find /usr/lib -name '*.so' | head -50" }
```

## Result shape

```json
{
  "stdout": "...",
  "stderr": "...",
  "exit_code": 0,
  "elapsed_secs": 0.42,
  "timed_out": false
}
```

`timed_out: true` means the wall-clock cap fired and the child was
sent SIGKILL — `exit_code` is then implementation-defined.

## When to prefer other tools

- **Reading a single file**: use `read_file` (cleaner, UTF-8 safe,
  capped at 1 MiB).
- **Writing a single file**: use `write_file` (atomic, creates parent
  directories).
- **Fetching a URL**: use `web_fetch` (timeouts, body cap).
- **Sending a reply to the user**: use `send_message`, never `echo`.

`shell` is the right tool when you genuinely need to compose multiple
operations, run an installed binary (compilers, formatters, linters),
or inspect the runtime environment.

## Safety model

The container is the sandbox. The tool itself trusts you with the
container — `rm -rf /`, `:(){ :|:& };:`, and other clichés are
container-bounded. The manager idle-stops the container after 5
minutes of quiet and crash-restarts it on heartbeat failure, so even
catastrophic damage resets on the next message.

What is **not** sandboxed:

- The bind-mounted session directory (`/data`) is the host-container
  IPC channel. Do not write garbage there — it contains
  `inbound.db` and `outbound.db`. The runner's own writes already
  scope to the right files.
- Outbound network reaches the host's network namespace by default.
  Operators can constrain this per-group via `egress_allow` (see
  `docs/container-config.md`); if you get connection errors hitting
  domains you used to reach, that's the cause.
