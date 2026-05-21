# Release 0.1.0 checklist

Use this checklist to drive an Ironclaw release. The numbers reset
for each release; "0.1.0" below is illustrative.

---

## 1. Preflight

- [ ] On `main`, no uncommitted changes.
- [ ] `cargo fmt --all -- --check` clean.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo test --workspace` green on Linux and macOS (CI is the
      source of truth; reproduce locally if anything looks off).
- [ ] `cargo llvm-cov --workspace --fail-under-lines 85` clean.
- [ ] No `#[ignore]` markers left in test files unless documented in
      `PLAN.md` (the audit query is `git grep -n '#\[ignore' crates/`).
- [ ] No `unsafe_code` introduced (`unsafe_code = "forbid"` is the
      workspace lint; verify with
      `cargo clippy --workspace --message-format=json | jq '.message | select(.code=="unsafe_code")'`).
- [ ] No `TODO` or `FIXME` in code that should block ship
      (`git grep -n 'TODO\|FIXME' crates/`). Open issues for any
      that remain.

## 2. Schema and migrations

- [ ] Every migration in `crates/ironclaw-db/migrations/` applies
      cleanly against an empty DB.
- [ ] No migration was reordered or rewritten — only **new** files
      were added. If you renamed one, that is a footgun: roll it back
      and add a fresh follow-up migration instead.
- [ ] `CentralDb::open` is idempotent — re-opening a migrated DB
      does nothing.
- [ ] Per-session migrations (`SessionInbound`, `SessionOutbound`)
      similarly only appended.
- [ ] The `--migrate-from` path tested against a real predecessor
      snapshot. The cutover guide
      (`docs/cutover.md`) describes the operator-visible flow.

## 3. Channel surface

- [ ] Every channel registered in
      `crates/ironclaw-host/src/channels_init.rs::build_registry`
      has a `register()` function exported from its crate root.
- [ ] `build_registry_has_every_in_tree_channel` passes.
- [ ] Each channel crate's README-equivalent docs (in `lib.rs`'s
      module-level comment) describes:
      - the `platform_id` shapes accepted,
      - which operations return `AdapterError::Unsupported`,
      - any persisted state files under `data_dir/`.
- [ ] Deferred channels (per `PLAN.md` M8) are documented either as
      checked with scope notes or unchecked with a reason.

## 4. Documentation

- [ ] `README.md` reflects the current status (`Pre-alpha` →
      `0.1.0` candidate, or whatever the next state is).
- [ ] `PLAN.md` Progress section ticked accurately. Each tick
      references the artifact that landed it.
- [ ] `docs/adding-a-channel.md` is up to date with the actual
      trait surface in `crates/ironclaw-channels/core/`.
- [ ] `docs/cutover.md` reflects the actual binary names and CLI
      flags.
- [ ] `docs/replay-fixtures.md` describes the harness that exists
      (not the one that was hoped for).
- [ ] `CHANGELOG.md` has an entry for this release. Format below.

## 5. Versioning

- [ ] `Cargo.toml` `workspace.package.version` matches the
      release. All member crates inherit via `version.workspace = true`.
- [ ] `Cargo.lock` regenerated and committed.
- [ ] `rust-toolchain.toml` `channel` matches the minimum required
      Rust (`rust-version = "1.85"` in `workspace.package` is the
      authoritative floor).

## 6. Binary artifacts

- [ ] `cargo build --release --workspace` produces:
      - `target/release/ironclaw` (host)
      - `target/release/iclaw` (admin client)
      - `target/release/ironclaw-setup` (setup helper)
- [ ] Binaries `--version` strings include the release version.
- [ ] `ironclaw run --check` exits 0 against a fresh empty data
      directory.
- [ ] `iclaw groups list` against a host with one group returns one
      row.

## 7. Tag and publish

- [ ] Create the annotated tag: `git tag -a v0.1.0 -m "0.1.0"`.
- [ ] Push the tag: `git push origin v0.1.0`.
- [ ] Release notes copied from the matching `CHANGELOG.md` entry.
- [ ] (Optional) `cargo publish` for any sub-crate intended for
      external consumption. Most workspace crates are internal and
      should not be published.

## 8. Post-release

- [ ] Open an issue tracking the next milestone's known follow-ups.
- [ ] Bump `workspace.package.version` to the next dev version
      (e.g., `0.2.0-dev`) on `main` so artifacts built between
      releases are unambiguous.

---

## CHANGELOG format

```
## [0.1.0] - 2026-MM-DD

### Added
- Initial Ironclaw release. ...

### Changed
- ...

### Fixed
- ...

### Known limitations
- ...
```

One section per release. Versions are linked at the bottom of the
file in `[Unreleased]: ...`, `[0.1.0]: ...` form. Follow [Keep a
Changelog](https://keepachangelog.com/) for the section vocabulary
(`Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`).
