//! Name-drift guard (M18 R0).
//!
//! Every tool name referenced by the runner's policy allow-lists
//! (`copperclaw_runner::policy::PROFILE_TOOL_LISTS`) must exist in the
//! real in-container inventory (`copperclaw_mcp::build_tool_set()`) or be
//! one of the two host-brokered preview tools that are dispatched via the
//! reserved `__preview` relay instead of the in-process tool map
//! (`copperclaw_runner::run::preview`).
//!
//! The old `DISALLOWED_TOOLS` floor rotted into a decorative list exactly
//! because nothing pinned its pascal-case names to the `snake_case`
//! inventory — the names never matched, so the floor never denied a real
//! call. This test makes the same drift in the live profile lists a CI
//! failure: a profile referencing a renamed or removed tool fails here.

use std::collections::HashSet;

use copperclaw_runner::policy::PROFILE_TOOL_LISTS;
use copperclaw_runner::run::preview::{CLOSE_PREVIEW, EXPOSE_PREVIEW, MAKE_PREVIEW_PUBLIC};

/// The advertised names of every in-process tool.
fn in_process_inventory() -> HashSet<String> {
    copperclaw_mcp::build_tool_set()
        .iter()
        .map(|e| e.tool.name.to_string())
        .collect()
}

/// Policy-known tools that are deliberately NOT in the in-process tool
/// map: the M17 preview pair and the M19 A3 public-tunnel verb all ride the
/// host-broker `__preview` relay (see `copperclaw-runner/src/run/preview.rs`),
/// so `build_tool_set()` never registers a handler for them.
fn host_brokered() -> HashSet<&'static str> {
    [EXPOSE_PREVIEW, CLOSE_PREVIEW, MAKE_PREVIEW_PUBLIC]
        .into_iter()
        .collect()
}

#[test]
fn every_policy_referenced_tool_exists() {
    let inventory = in_process_inventory();
    let brokered = host_brokered();
    for (list, names) in PROFILE_TOOL_LISTS {
        for name in *names {
            assert!(
                inventory.contains(*name) || brokered.contains(name),
                "policy list {list} references `{name}`, which is neither in \
                 copperclaw_mcp::build_tool_set() nor a host-brokered preview tool \
                 — rename it or remove it so the allow-list stays real"
            );
        }
    }
}

#[test]
fn no_duplicate_names_within_a_policy_list() {
    // A duplicated entry is harmless at runtime but always a copy-paste
    // mistake; catch it here where the failure message names the list.
    for (list, names) in PROFILE_TOOL_LISTS {
        let mut seen = HashSet::new();
        for name in *names {
            assert!(seen.insert(*name), "policy list {list} repeats `{name}`");
        }
    }
}

#[test]
fn host_brokered_exceptions_stay_out_of_the_in_process_inventory() {
    // If `expose_preview` / `close_preview` ever become in-process tools,
    // the exception set in this test must be deleted rather than silently
    // shadowing a real registration.
    let inventory = in_process_inventory();
    for name in host_brokered() {
        assert!(
            !inventory.contains(name),
            "`{name}` is now registered in build_tool_set(); drop it from the \
             host-brokered exception set in this test"
        );
    }
}
