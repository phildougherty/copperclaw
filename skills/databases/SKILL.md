---
name: databases
description: Run-books for installing and running real database servers (Postgres, MariaDB/MySQL, Redis, MongoDB) inside the session container as a non-root user with data under /data. Opt-in for coding agents; use when a build needs a real database, not SQLite.
---

# databases

How to stand up a real database server inside your session container.
Reach for this when [[web-backend]]'s datastore choice lands on
Postgres/MySQL/Redis/Mongo instead of SQLite — concurrent writers,
relational integrity, production-shaped stacks, or the user asked for
a specific engine by name.

The environment you are working in:

- You are **uid 1000, not root**, with **no /etc/passwd entry**. No
  systemd, no `service`, no `sudo`. Start servers as plain background
  processes.
- **Probe before you bake**: groups on the `backend` image profile
  already ship Postgres, MariaDB, and Redis — check `command -v
  initdb mariadbd redis-server` first. Server packages are apt, and
  apt only installs at image-build time: `install_packages` with
  `scope: "image"` bakes them into a *future* session, never this one
  (see [[install-packages]]). Request them the moment you know you'll
  need them, then continue with SQLite or a tarball fallback this
  session. Client libraries are different — a pip driver or npm
  client lands live, this turn, via `scope: "session"`.
- Keep every data dir, socket, and pid file under `/data` — it is the
  only writable, session-persistent path. `/var/lib/*` and `/run/*`
  are root-owned; pointing a server at them is the #1 failure.
- **Servers die when the container idle-stops; `/data` survives.** On
  a fresh container your data is intact but nothing is running — so
  write your exact start commands to `/data/.copperclaw/services`
  (one bash command per line; `#` comments and blank lines skipped).
  The runtime runs that file once on every container boot, appending
  each command's output and exit status to
  `/data/.copperclaw/services.log`. When a connection is refused,
  read that log first — don't debug a "down" DB whose restart simply
  failed.
- Bind to `127.0.0.1`. Your app connects over localhost; nothing
  outside the container needs the port.

## Postgres

Bake: `install_packages` with `apt: ["postgresql",
"postgresql-client"]`. The image baseline ships `libnss-wrapper`
(`initdb` refuses a uid with no passwd entry without it) — if
`/usr/lib/x86_64-linux-gnu/libnss_wrapper.so` is missing you are on an
older image; add `"libnss-wrapper"` to the apt list too.

```bash
# one-time init
echo "agent:x:$(id -u):$(id -g):agent:/data:/bin/bash" > /data/passwd
echo "agent:x:$(id -g):" > /data/group
export NSS_WRAPPER_PASSWD=/data/passwd NSS_WRAPPER_GROUP=/data/group
export LD_PRELOAD=/usr/lib/x86_64-linux-gnu/libnss_wrapper.so
export PATH=$(ls -d /usr/lib/postgresql/*/bin | head -1):$PATH
initdb -D /data/pg --auth=trust -U agent
# start — wrap the exports above + this line in /data/start-pg.sh and
# add "bash /data/start-pg.sh" to /data/.copperclaw/services
pg_ctl -D /data/pg -l /data/pg/log -o "-k /data/pg -p 5432 -h 127.0.0.1" start
psql -h 127.0.0.1 -U agent -d postgres -c 'select 1'
```

`DATABASE_URL=postgres://agent@127.0.0.1:5432/postgres`. Unset
`LD_PRELOAD` before running unrelated commands.

## MariaDB (the MySQL of Debian)

Debian ships MariaDB; asked for "MySQL", use it — wire-compatible.
Bake: `apt: ["mariadb-server", "mariadb-client"]`.

```bash
# one-time init
mariadb-install-db --datadir=/data/mysql \
  --auth-root-authentication-method=normal --skip-test-db
# start (add to /data/.copperclaw/services) — socket + pid under /data
mariadbd --datadir=/data/mysql --socket=/data/mysql/mysql.sock \
  --pid-file=/data/mysql/mysqld.pid --port=3306 \
  --bind-address=127.0.0.1 >/data/mysql/server.log 2>&1 &
mariadb --socket=/data/mysql/mysql.sock -u root -e 'select 1'
```

Without `--pid-file` under `/data` it dies on read-only `/run/mysqld`
*after* logging a healthy startup — check the log tail, not just that
the process launched.

## Redis

Bake: `apt: ["redis-server"]`. No init step:

```bash
mkdir -p /data/redis
redis-server --daemonize yes --dir /data/redis --port 6379 --bind 127.0.0.1
redis-cli ping
```

Cache/sessions/queues only — pair it with a real system of record.

## MongoDB

Not in Debian's repos — no apt path. Download the official tarball
into `/data` (subject to the group's egress policy; if the download is
refused, say so and offer SQLite/Postgres instead):

```bash
curl -fsSL https://fastdl.mongodb.org/linux/mongodb-linux-x86_64-debian12-8.0.4.tgz | tar -C /data -xz
mkdir -p /data/mongo
/data/mongodb-linux-*/bin/mongod --dbpath /data/mongo \
  --bind_ip 127.0.0.1 --port 27017 --fork --logpath /data/mongo/log
# client shell, if needed:
curl -fsSL https://downloads.mongodb.com/compass/mongosh-2.3.8-linux-x64.tgz | tar -C /data -xz
```

## Wiring it into the build

- App config reads `DATABASE_URL` / `REDIS_URL` from the environment
  ([[web-backend]] rule) — the URLs above, never hardcoded.
- Add each server's start command to `/data/.copperclaw/services`
  *and* a health check to `.copperclaw/verify` (e.g. `db: psql -h
  127.0.0.1 -U agent -d postgres -c 'select 1'`) so the verify gate
  catches a dead server before you claim "done" ([[coding-task]]).
- Seed/migrate via scripts, not by hand — the datadir persists, but a
  reproducible schema beats an artisanal one.

## Related skills

[[web-backend]] (choosing the datastore), [[install-packages]] (baking
the server packages), [[coding-task]] (verify gate), [[debug]]
(reading a server log tail).
