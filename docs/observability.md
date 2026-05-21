# Observability

Ironclaw exposes two opt-in observability surfaces, both off by default
per the conservative-defaults tenet. Enable them by setting the
relevant environment variable before launching `ironclaw run`.

## Prometheus metrics endpoint

Set `IRONCLAW_METRICS_ADDR` to start a small `/metrics` HTTP server on
boot. Accepts either a full `host:port` pair or a bare port (which
auto-prefixes to `127.0.0.1:` so a typo never opens a public listener
by accident).

```
# Loopback only — bare port shorthand.
export IRONCLAW_METRICS_ADDR=9090

# Or explicit host:port.
export IRONCLAW_METRICS_ADDR=127.0.0.1:9090

# All interfaces (do this only when the port is behind a reverse proxy
# that enforces authn/authz).
export IRONCLAW_METRICS_ADDR=0.0.0.0:9090
```

Bind failures log a warning and the host continues to boot — metrics
is never a hard dependency. Malformed addresses log
`IRONCLAW_METRICS_ADDR is malformed, metrics endpoint disabled: ...`
and the endpoint stays off; the rest of the host runs normally.

### What is exported

#### Counters

| Name | Labels | Bumped by |
|---|---|---|
| `ironclaw_messages_inbound_total` | `channel_type` | router, after a successful inbound DB write |
| `ironclaw_messages_outbound_total` | `channel_type` | delivery loop, on successful adapter dispatch |
| `ironclaw_delivery_failed_total` | `channel_type` | delivery loop, on final-failure (3 retries exhausted) |
| `ironclaw_containers_spawned_total` | none | container manager, after a successful `runtime.spawn` |
| `ironclaw_containers_crashed_total` | none | container manager, on `CrashRestart` (heartbeat stale) |
| `ironclaw_image_rebuild_failed_total` | none | container manager, when `runtime.build_image` errors (the spawn falls back to the last-known-good tag) |

#### Histograms

| Name | Unit | Recorded by |
|---|---|---|
| `ironclaw_llm_call_seconds` | seconds | runner, after each `run_llm_turn` |
| `ironclaw_llm_tokens_input` | tokens | runner, from `ProviderEvent::Usage` |
| `ironclaw_llm_tokens_output` | tokens | runner, from `ProviderEvent::Usage` |
| `ironclaw_container_spawn_seconds` | seconds | container manager, around `runtime.spawn` |

### Recommended alerts

- `rate(ironclaw_containers_crashed_total[5m]) > 0` — runners dying;
  inspect host stderr / `IRONCLAW_LOG_DIR` for the cause.
- `rate(ironclaw_image_rebuild_failed_total[1h]) > 0` — operator-
  supplied config (`packages_apt`, `packages_npm`) is broken; the
  group is still serving requests on a stale image. Fix the config or
  the agent loses future package contributions.
- `rate(ironclaw_delivery_failed_total[15m]) > 0` — a channel is
  dropping messages after 3 retries; check the channel's auth /
  rate-limit headers via the delivery logs.

### Scrape config

```yaml
# prometheus.yml
scrape_configs:
  - job_name: ironclaw
    static_configs:
      - targets: ["127.0.0.1:9090"]
```

The endpoint returns text/plain `# HELP` + `# TYPE` headers followed
by the per-metric samples, exactly as Prometheus expects.

## Log rotation

Without configuration, `tracing` writes to stderr. For a long-lived
daemon this grows the unit's journal or your `systemd` log target
unbounded. Set `IRONCLAW_LOG_DIR` to fan tracing output to a
daily-rotating file as well, while keeping the stderr writer for unit
captures.

```
export IRONCLAW_LOG_DIR=/var/log/ironclaw
```

The file naming convention is `<dir>/host.log.<YYYY-MM-DD>`. Files
are not auto-deleted — the daily rotation never shrinks the directory.
For retention, layer your platform's standard tool on top:

```
# /etc/logrotate.d/ironclaw — example, paired with IRONCLAW_LOG_DIR=/var/log/ironclaw
/var/log/ironclaw/host.log.* {
    weekly
    rotate 8
    compress
    delaycompress
    missingok
    notifempty
}
```

`IRONCLAW_LOG` (the existing `tracing-subscriber` env filter) still
controls the verbosity, e.g. `IRONCLAW_LOG=info,ironclaw_host=debug`.
Filter changes apply to both the stderr writer and the rolling file
writer.

## What is **not** covered yet

- No distributed-tracing (OpenTelemetry) export. The data is in
  `agent_turns` + `audit_log` and can be queried directly.
- No structured per-request span IDs propagated across the host /
  container boundary. The session id is the closest correlation
  handle today.
- Log rotation file naming is fixed to `host.log.<date>`. The
  underlying `tracing-appender` crate supports hourly rotation;
  Ironclaw deliberately exposes only the daily knob to keep the
  surface small.
