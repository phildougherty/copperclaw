# Database backup and restore

The central database `<data_dir>/ironclaw.db` is a single SQLite file
holding every agent group, messaging-group wiring, audit-log row,
token-usage record, sender approval, and per-group budget. A loss of
this file requires re-seeding all of that state by hand. The
operational story is straightforward but has one important sharp edge.

## Backup — `iclaw db backup <path>`

Routes through the iclaw socket so the action lands in `audit_log`.
Steps the handler runs, in order:

1. `PRAGMA wal_checkpoint(TRUNCATE)` against the open host connection
   to drain the write-ahead log into the main database file. Best-
   effort: if a concurrent transaction holds the WAL busy, the
   checkpoint is logged and the copy proceeds with whatever drained
   — the backup is still a valid SQLite file, just with some WAL
   data that replays on next open.
2. Locate the source file via the open connection's path.
3. Copy to `<path>.tmp` next to the destination (so the rename is
   atomic on the same filesystem). Parent directories are auto-
   created.
4. `rename(2)` the temp file into place.

Example:

```
# Stop-the-world is NOT required.
iclaw db backup /var/backups/ironclaw-$(date +%F).db

# Returns: { "path": "...", "wal_pages_remaining": 0 }
# wal_pages_remaining > 0 means the WAL didn't fully drain.
```

The handler errors are:

| Code | Meaning |
|---|---|
| `bad_request` | `path` argument missing or empty |
| `in_memory_db` | the host is using `:memory:` (test fixture only) |
| `io_error` | filesystem failure during checkpoint / copy / rename |

The backup file is a self-contained SQLite database — you can open it
read-only with `sqlite3 backup.db .dump` to inspect, or restore it
into a different install by overwriting that install's
`<data_dir>/ironclaw.db` while its host is stopped.

### Cadence

There is no built-in scheduler. Pair `iclaw db backup` with a cron
job or systemd timer:

```
# /etc/systemd/system/ironclaw-backup.timer
[Unit]
Description=Nightly Ironclaw central DB backup

[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

```
# /etc/systemd/system/ironclaw-backup.service
[Unit]
Description=Ironclaw central DB backup

[Service]
Type=oneshot
ExecStart=/usr/local/bin/iclaw db backup /var/backups/ironclaw/ironclaw-%i.db
```

Pair with the logrotate-style retention policy you already use for
`<data_dir>/logs/` (see `docs/observability.md`).

## Restore — why the iclaw command always refuses

`iclaw db restore <path>` exists in the CLI for discoverability, but
the handler **always** returns:

```
host_running: db restore cannot run while the host is active...
```

The reason is structural: the host process holds an open WAL
connection to `<data_dir>/ironclaw.db`. Replacing the file underneath
that connection would corrupt the database — the WAL header would no
longer match the main file's page count, and the next write would
race a concurrent reader against a stale page cache.

The correct procedure is **offline restore**:

```
# 1. Stop the host. systemd:
sudo systemctl stop ironclaw

# Or, manual (preferred — handles SIGTERM grace + SIGKILL fallback):
ironclaw stop
# Or if you have to do it by PID:
kill "$(cat <data_dir>/ironclaw.pid)"
# Wait until `ironclaw status` reports stopped and the iclaw.sock
# file is gone.

# 2. Copy the backup over the live file.
sudo cp /var/backups/ironclaw-2026-05-21.db /srv/ironclaw/data/ironclaw.db

# 3. (Optional) re-apply migrations against the restored file.  This
# is a no-op if the backup was taken from a release with the same
# schema version, and forward-compatible-fills any gap if the backup
# came from an older release.
sudo -u ironclaw ironclaw migrate

# 4. Restart.
sudo systemctl start ironclaw
```

## What is **not** in the backup

The central DB is the only thing `iclaw db backup` captures. To restore
a complete install you also need:

- `<data_dir>/sessions/` — per-session `inbound.db` / `outbound.db`
  files plus each session's `inbox/` and `outbox/` directories (the
  attachment payload bind-mounts). These are typically transient
  (each session's history fits inside the compaction window), but if
  you want fully reproducible state, back them up with the standard
  filesystem tools.
- `.env` — the API key + paths. Treated as a secret; back up
  separately using your normal secret-store flow.
- Container images. The session image is rebuilt from
  `container_configs` automatically on the first spawn after restore
  (see [docs/container-config.md](container-config.md)), so this is
  derivable, not load-bearing.
