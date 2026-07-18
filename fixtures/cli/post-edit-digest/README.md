# cli/post-edit-digest — M22 CX (Wave-1 coding X-rider)

Deterministic replay coverage for the **C1 post-edit verify hook**: after a
successful `write_file`/`edit_file`/`multi_edit`/`apply_patch` mutation the
runner auto-runs the applicable format/typecheck checker scoped to the touched
file and appends a concise digest under `post_edit_diagnostics` in the tool
result, so type/lint breakage feeds back to the *model* next turn instead of
leaking to the user (`crates/copperclaw-mcp/src/tools/diagnostics.rs`,
`post_edit_verify` / `append_post_edit_digest`).

## What it drives

A single cli/stdin coding turn:

1. **claude/001** — `write_file` a broken `index.ts`
   (`const answer: number = 'nope';`). The mutation site in
   `computer_use.rs` calls `append_post_edit_digest`, which sniffs `.ts` →
   `tsc`, runs it on the file, parses the output, and folds the diagnostics
   into the tool result.
2. **claude/002** — a closing acknowledgement. Its provider request body is
   where the fed-back digest is asserted (the digest rides back to the model
   inside the prior turn's `write_file` `tool_result`, exactly like the
   verify-gate refusals ride back as tool-result text).

## Why a fake `tsc` (and a re-exec)

The post-edit hook runs a **real** checker subprocess (`tsc`/`eslint`/`ruff`),
probed at call time via `command -v`. The replay/CI environment has none of
them, so `bin/tsc` is a deterministic stand-in that emits one canonical
`TS2322` line. `parse_tsc_output` + `build_post_edit_digest` then produce a
digest **byte-identical** to the committed golden
`fixtures/diagnostics/post-edit-digest/recorded-digest.json` — the registered
test asserts that golden's `note` string appears verbatim in the provider
request body, tying this replay fixture to the C1 unit-test golden.

`bin/` is put on `PATH` for the harness run via a subprocess **re-exec** — the
same seam the `prototype-verify-gate` fixtures use for `COPPERCLAW_DATA_ROOT`,
because the workspace `forbid(unsafe_code)` blocks `std::env::set_var`. The
parent test spawns the child with `PATH=<fixture>/bin:$PATH`; the child boots
the real `ReplayHarness`, so the hook fires against a genuinely-resolved `tsc`.

## Registered test

`tests/replay.rs::cli_post_edit_digest_feeds_diagnostics_back` — parent/child
re-exec; the child byte-diffs the four expected streams AND asserts the digest
(hook name, `TS2322`, the golden `note`, the type-mismatch message) reached the
model via the captured provider request bodies. Regenerate the expected
streams with `COPPERCLAW_CX_GENERATE=1 cargo test -p copperclaw-host --test
replay cli_post_edit_digest_feeds_diagnostics_back -- --nocapture`.
