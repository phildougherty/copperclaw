# Cutover guide

This guide walks an operator through replacing a running predecessor
installation with the Ironclaw Rust host. The two systems are
intentionally schema-compatible: Ironclaw reads the same `ironclaw.db`
central schema, the same per-session `inbound.db` / `outbound.db`
layout, and the same `data/sessions/<agent-group>/<session>/`
folder structure as the predecessor.

The cutover is a swap, not a transform. Most of the work is making
sure nothing is mid-flight when you flip the switch.

---

## 1. Pre-flight

Before you begin:

- **Pin a version.** Build or download the exact `ironclaw` binary
  you intend to deploy. Verify `ironclaw version` reports a version
  you have changelog notes for.
- **Read `docs/adding-a-channel.md`** if you operate a channel that
  is not yet covered. Cutover does not regress channel coverage —
  any platform you used in the predecessor must have an in-tree
  channel crate before you cut over.
- **Confirm container runtime.** Run `docker info` (Linux) or
  `container --help` (macOS). Ironclaw will not start if its
  configured runtime is unreachable.
- **Capture a known-good baseline.** Tail the predecessor's logs and
  the host metrics dashboard. Record the in-flight session count,
  pending approvals count, and the last delivered message id per
  channel.

## 2. Quiesce

The cutover is safest when both systems agree there is nothing
in-flight.

1. **Stop accepting new inbound work.** Disable channel ingress at
   the source where you can:
   - Telegram: unset the webhook (`POST /deleteWebhook`).
   - Slack: pause the events subscription URL.
   - Discord: leave the gateway disconnected (kill the bot session).
   - Webex / GitHub / Linear / Teams / Webex / Matrix / Resend:
     disable the webhook at the platform side.
   - Long-poll channels (Telegram long-poll, Matrix sync, X DM poll):
     stop the predecessor and they stop polling.
2. **Let the predecessor drain.** Watch the active session count
   trend to zero. Sessions in `running` should finish their current
   turn; sessions in `idle` should stay idle.
3. **Snapshot.** With the predecessor stopped, copy the entire data
   directory to a known-good location. The minimum is `data/ironclaw.db`
   plus `data/sessions/`, but copying the whole tree (logs
   included) preserves diagnostic state.

   ```
   cp -a /var/lib/ironclaw-old /var/lib/ironclaw-old.snapshot.$(date +%Y%m%dT%H%M%S)
   ```

   The snapshot is your rollback target.

## 3. Migrate

Run the data-directory migrator. This is idempotent — running it
twice on the same destination is safe.

```
ironclaw setup --migrate-from /var/lib/ironclaw-old \
               --data-dir /var/lib/ironclaw
```

The migrator:

- Copies `<source>/data/ironclaw.db` to `<dest>/data/ironclaw.db`.
- Calls `CentralDb::open`, which runs every central migration in
  `crates/ironclaw-db/migrations/` against the copy. Migrations are
  recorded in `schema_migrations`; re-running is a no-op.
- Leaves per-session DBs in place. Session DBs are migrated lazily
  the first time the new host opens them, via the
  `SessionInbound` / `SessionOutbound` migration sets.

If the migrator reports `copied_db: false` and you expected a copy,
re-check the `--migrate-from` path — it should point at the
predecessor's **data root**, the directory that contains `data/ironclaw.db`.

## 4. Verify

Do not start the channel ingress yet. Run a read-only verification
pass first.

```
# Sanity: the host can boot and exit cleanly.
ironclaw run --once --check

# Schema introspection via iclaw on an idle host.
ironclaw run &
HOST_PID=$!
iclaw groups list
iclaw sessions list --status active
iclaw approvals list
kill $HOST_PID
wait $HOST_PID
```

What you are looking for:

- `groups list` returns the same row count as the predecessor.
- `sessions list --status active` is empty (you quiesced everything).
- `approvals list` matches your pre-cutover dashboard count.
- The host process logs no migration errors and no `journal_mode`
  warnings (silent data loss happens when WAL leaks across the
  mount; the new host enforces `journal_mode=DELETE` for inbound
  DBs, but a stale `inbound.db-wal` from the predecessor will be
  noticed in logs).

If anything is off, do not proceed — go to **Rollback**.

## 5. Switch ingress

Bring the new host up under your service supervisor first
(`systemctl`, `launchctl`, or your supervisor of choice), then
re-enable channel ingress one platform at a time:

1. CLI channel (if enabled) — type into the host's stdin and see a
   container respond. This proves the runner-side pipeline.
2. The lowest-traffic platform first. Re-set the webhook (or
   re-enable the gateway). Watch for the first inbound event in the
   host log.
3. Each subsequent platform after the previous one has seen a
   successful round-trip.

The order matters because each platform has its own ingress quirks
(webhook signature, replay-id dedup, resume tokens). Bringing them
up serially makes the failure mode obvious.

## 6. Watch the first hour

Things to monitor for an hour after cutover:

- **Active sessions trend.** Should rise to roughly the
  predecessor's steady state.
- **Delivery loop lag.** `outbound.processing_ack` rows should
  transition out of `pending` within seconds. A stuck `pending` row
  past the sweep interval means delivery is broken for that
  channel.
- **Approvals.** Any pre-existing `pending_*_approvals` rows should
  still be addressable via `iclaw approvals get <id>`.
- **Container restarts.** If the host kills a container as "stuck"
  in the first 10 minutes, that is the heartbeat / processing-ack
  check working as intended, but a sustained restart loop indicates
  the container image or skill mount is wrong.

## 7. Rollback

Rollback is a swap back, never a transform:

1. Stop the new host (`systemctl stop ironclaw`).
2. Disable channel ingress again at the platform side.
3. Replace `/var/lib/ironclaw` with the snapshot from step 2.
4. Start the predecessor.
5. Re-enable channel ingress.

The snapshot is the only safe source of truth — do **not** try to
selectively roll back tables or attempt to fix the migrated DB
in-place. The schema may have advanced via migrations, and walking
back live SQL is a known footgun. Snapshot, swap, move on.

---

## Common cutover pitfalls

- **Forgetting `journal_mode=DELETE`.** If you copy an inbound DB
  while WAL files still exist on disk, the new host will keep the
  WAL mode unless you delete `inbound.db-wal` and `inbound.db-shm`
  alongside it. The migrator does not touch session DBs, so this is
  your responsibility for any session that was open at quiesce
  time. Easiest fix: stop the predecessor cleanly so WAL files are
  checkpointed and removed before snapshot.
- **Skill content paths.** The predecessor and Ironclaw both expect
  skill content under `skills/`. If you ran the predecessor with a
  custom path, re-symlink before the new host's first boot — the
  skill discovery runs at agent-startup and an empty skill set
  silently degrades behavior.
- **OneCLI credential cache.** OneCLI tokens live outside the data
  directory (`~/.config/onecli/`). Cutover preserves them. If you
  rotate them, rotate via `onecli login` after the cutover, not
  during.
- **systemd unit naming.** The generated unit is `ironclaw.service`.
  If the predecessor's unit was also named `ironclaw.service`,
  disable and remove the old one before `daemon-reload` to avoid a
  startup race.
- **Time skew.** The sweep loop uses wall-clock `Utc::now()` to
  detect stuck containers. If you cut over across a clock-adjust
  event (NTP catching up after an extended outage), brace for a
  burst of "stuck" detections in the first sweep cycle. They will
  self-correct.
