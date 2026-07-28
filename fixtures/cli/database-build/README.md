# cli / database-build

M23 W3.3: the databases capability path, driven deterministically through
the replay pipeline — no docker-in-docker, no network, no real postgres
process anywhere in the run.

One CLI inbound ("set up a postgres database for the app") drives three
scripted Claude turns:

1. `install_packages` — apt `postgresql` + `postgresql-client` with a
   reason, `scope: "image"`. The tool emits an `install_packages` System
   row into `messages_out` (nothing installs in-turn — apt packages bake
   at the NEXT container spawn, the contract `skills/databases` teaches).
   The delivery loop's host-side apply path (`apply_install_packages` in
   `copperclaw-host-delivery`) then merges both packages into central
   `container_configs.packages_apt` — the config the next spawn's image
   bake reads. The registered test asserts the applied central state on
   top of the byte-stable JSONL diff.
2. `write_file` — registers the run-book's `pg_ctl ... start` line in
   `.copperclaw/services`, the file the W2.2 cold-boot hook
   (`copperclaw-runner/src/run/services.rs`) replays line-by-line at the
   next container boot. In production this file lives at
   `/data/.copperclaw/services`; the in-process harness runs no container
   and `/data` is an unwritable root-owned path on any host running the
   suite, so the scripted turn writes the same `.copperclaw/services`
   layout under the fixed stand-in root
   `/tmp/copperclaw-m23-database-build` (the `transcript-render` /
   `prototype-golden` precedent). The registered test wipes the root
   before the run (so the write is always a fresh create — an overwrite
   would emit a Diff card row and break the byte-stable streams), reads
   the file back after, and removes the root again.
3. Final text reply, delivered through the `MockAdapter`.

What this fixture deliberately does NOT exercise, and why:

- The W2.2 cold-boot service-restart hook itself: it runs once per real
  container boot in `run_loop`'s cold-start path; the harness's
  in-process per-step runner has no container boot to hook. Covered by
  `services.rs`'s own unit tests.
- The W2.4 verify-gate `db:` stage: it is written at project attach and
  checked as a live `/dev/tcp` reachability probe — both need a project
  and a listening socket the deterministic replay cannot honestly
  provide. Covered by `project.rs`'s own unit tests.

Regenerate `expected/*.jsonl` after an intentional pipeline change with:

```
COPPERCLAW_M23W3_GENERATE=1 cargo test -p copperclaw-host --test replay \
  cli_database_build_installs_packages_and_registers_services -- --exact --nocapture
```
