//! `cclaw egress list-presets` / `cclaw egress allow <preset>` — curated
//! egress allow-list presets.
//!
//! Under the opt-in `deny-default` egress posture, a container can only
//! reach `host:port` pairs on its group's allow-list. Some workloads need
//! a small, well-known set of endpoints (e.g. the databases run-book
//! downloads `MongoDB` tarballs) and making the operator hunt down the
//! exact hostnames defeats the "easy lockdown" goal. This module ships a
//! curated catalog — the egress twin of the MCP preset registry in
//! `copperclaw-host/src/handlers/mcp.rs` — so allowing a known endpoint
//! set is one command:
//!
//! ```text
//! cclaw egress list-presets
//! cclaw egress allow mongodb --agent-group-id <id>
//! ```
//!
//! Both are client-side composite ops (like `cclaw status` / `cclaw
//! doctor`): `list-presets` renders the compile-time catalog with no
//! socket round-trip, and `allow` is a read-merge-write over the existing
//! `groups.config.get` + `groups.config.set-egress-allow` wire commands.
//! Because the write goes through `set-egress-allow`, it stays host-only
//! and lands in `audit_log` like any hand-rolled allow-list edit — no new
//! wire surface, no second mutation path.
//!
//! Unlike `set-egress-allow` (which REPLACES the list), `allow` MERGES:
//! existing entries are preserved and preset entries are appended,
//! deduplicated. Re-running is a no-op.

use crate::client::ClientError;
use crate::protocol::Caller;
use crate::style::Palette;
use crate::{CallTransport, RunOutput, render_json_pretty, render_with};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Preset catalog
// ---------------------------------------------------------------------------

/// A single curated egress allow-list preset.
pub struct EgressPreset {
    /// Short identifier (used as `cclaw egress allow <name>`).
    pub name: &'static str,
    /// One-line description shown in `cclaw egress list-presets`.
    pub description: &'static str,
    /// The `host:port` pairs the preset allows.
    pub entries: &'static [&'static str],
}

impl EgressPreset {
    /// Render to the catalog entry shape used by `list-presets`.
    fn to_catalog_entry(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "entries": self.entries,
        })
    }
}

/// The curated egress preset catalog. Adding a new entry here is the only
/// change required to ship a new preset — `cclaw egress allow <name>`
/// picks it up automatically.
///
/// Catalog rules:
///
/// 1. Every entry is a literal `host:port` pair — the same shape
///    `groups.config.set-egress-allow` validates host-side. No wildcards,
///    no bare hosts.
/// 2. Entries are the MINIMAL set for the named workload. A preset is a
///    convenience, not a bundle of "probably useful" endpoints.
pub const EGRESS_PRESETS: &[EgressPreset] = &[EgressPreset {
    name: "mongodb",
    description: "MongoDB tarball downloads: fastdl.mongodb.org (server), \
                  downloads.mongodb.com (mongosh).",
    entries: &["fastdl.mongodb.org:443", "downloads.mongodb.com:443"],
}];

/// Look up a preset by name. Returns `None` if the name is not in the
/// catalog.
pub fn find_egress_preset(name: &str) -> Option<&'static EgressPreset> {
    EGRESS_PRESETS.iter().find(|p| p.name == name)
}

// ---------------------------------------------------------------------------
// Composite runners
// ---------------------------------------------------------------------------

/// `cclaw egress list-presets` — render the compile-time catalog. No
/// transport involved.
pub(crate) fn run_egress_list_presets(as_json: bool, palette: Palette) -> RunOutput {
    let catalog: Vec<Value> = EGRESS_PRESETS
        .iter()
        .map(EgressPreset::to_catalog_entry)
        .collect();
    let data = Value::Array(catalog);
    let mut text = if as_json {
        render_json_pretty(&data)
    } else {
        render_with(&data, palette)
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    RunOutput::success(text)
}

/// `cclaw egress allow <preset> --agent-group-id <id>` — merge the
/// preset's `host:port` entries into the group's existing allow-list.
///
/// Read-merge-write over the existing wire surface:
///
/// 1. `groups.config.get` for the current `egress_allow` (a missing
///    config row reads as an empty list).
/// 2. Union: existing entries first (order preserved), then any preset
///    entries not already present.
/// 3. `groups.config.set-egress-allow` with the merged list — skipped
///    entirely when every preset entry is already allowed, so re-running
///    never produces a redundant audit row.
pub(crate) async fn run_egress_allow_preset<T>(
    args: &Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
    palette: Palette,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let Some(preset_name) = args.get("preset").and_then(Value::as_str) else {
        return RunOutput::failure("egress allow: missing preset\n".to_string());
    };
    let Some(id) = args.get("agent_group_id").and_then(Value::as_str) else {
        return RunOutput::failure("egress allow: missing agent_group_id\n".to_string());
    };
    let Some(preset) = find_egress_preset(preset_name) else {
        let known: Vec<&str> = EGRESS_PRESETS.iter().map(|p| p.name).collect();
        return RunOutput::failure(format!(
            "egress allow: unknown preset `{preset_name}`; known presets: {}\n",
            known.join(", ")
        ));
    };

    // 1. Current allow-list. `groups.config.get` returns `null` when the
    // group has no config row yet — that reads as an empty list here, and
    // the host's set-egress-allow handler creates the row on write.
    let current = match transport
        .call("groups.config.get", json!({"id": id}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return fail_call("groups.config.get", &e),
    };
    let existing: Vec<String> = current
        .get("egress_allow")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    // 2. Union, preserving the operator's existing order.
    let added: Vec<String> = preset
        .entries
        .iter()
        .filter(|e| !existing.iter().any(|x| x == *e))
        .map(|e| (*e).to_string())
        .collect();

    let mut merged = existing;
    merged.extend(added.iter().cloned());

    // 3. Write back only when something changed.
    if !added.is_empty() {
        if let Err(e) = transport
            .call(
                "groups.config.set-egress-allow",
                json!({"id": id, "allow": merged}),
                caller,
            )
            .await
        {
            return fail_call("groups.config.set-egress-allow", &e);
        }
    }

    let data = json!({
        "agent_group_id": id,
        "preset": preset.name,
        "added": added,
        "egress_allow": merged,
    });
    let mut text = if as_json {
        render_json_pretty(&data)
    } else if data["added"].as_array().is_some_and(Vec::is_empty) {
        format!(
            "preset `{}` already allowed for {id} (no change)",
            preset.name
        )
    } else {
        render_with(&data, palette)
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    RunOutput::success(text)
}

/// Shape a transport failure into the composite's error output.
fn fail_call(command: &str, e: &ClientError) -> RunOutput {
    RunOutput::failure(format!("egress allow: {command} failed: {e}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // --- catalog integrity -------------------------------------------------

    #[test]
    fn mongodb_preset_covers_both_tarball_hosts() {
        let p = find_egress_preset("mongodb").expect("mongodb preset must exist");
        assert!(p.entries.contains(&"fastdl.mongodb.org:443"));
        assert!(p.entries.contains(&"downloads.mongodb.com:443"));
    }

    #[test]
    fn preset_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for p in EGRESS_PRESETS {
            assert!(seen.insert(p.name), "duplicate preset name: {}", p.name);
        }
    }

    #[test]
    fn every_preset_entry_is_host_port_shaped() {
        // Mirror the host-side `validate_egress_entry` contract: a
        // non-empty host, a colon, and a numeric port.
        for p in EGRESS_PRESETS {
            assert!(!p.entries.is_empty(), "preset `{}` has no entries", p.name);
            for e in p.entries {
                let (host, port) = e
                    .rsplit_once(':')
                    .unwrap_or_else(|| panic!("preset `{}` entry `{e}` lacks a port", p.name));
                assert!(!host.is_empty(), "preset `{}` entry `{e}`", p.name);
                assert!(
                    port.parse::<u16>().is_ok(),
                    "preset `{}` entry `{e}` port is not numeric",
                    p.name
                );
            }
        }
    }

    #[test]
    fn find_preset_lookup() {
        assert!(find_egress_preset("mongodb").is_some());
        assert!(find_egress_preset("not-a-preset").is_none());
    }

    // --- list-presets ------------------------------------------------------

    #[test]
    fn list_presets_json_carries_entries() {
        let out = run_egress_list_presets(true, Palette::plain());
        assert!(out.stderr.is_empty(), "{}", out.stderr);
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), EGRESS_PRESETS.len());
        let mongo = arr.iter().find(|e| e["name"] == "mongodb").unwrap();
        assert_eq!(mongo["entries"][0], "fastdl.mongodb.org:443");
        assert_eq!(mongo["entries"][1], "downloads.mongodb.com:443");
    }

    #[test]
    fn list_presets_human_mentions_preset_name() {
        let out = run_egress_list_presets(false, Palette::plain());
        assert!(out.stdout.contains("mongodb"));
    }

    // --- allow-preset ------------------------------------------------------

    /// Keyed transport: canned response per command name, records calls.
    struct KeyedTransport {
        responses: Vec<(&'static str, Value)>,
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl KeyedTransport {
        fn new(responses: Vec<(&'static str, Value)>) -> Self {
            Self {
                responses,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls_for(&self, command: &str) -> Vec<Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(c, _)| c == command)
                .map(|(_, a)| a.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for KeyedTransport {
        async fn call(
            &self,
            command: &str,
            args: Value,
            _caller: Caller,
        ) -> Result<Value, ClientError> {
            self.calls.lock().unwrap().push((command.to_string(), args));
            self.responses
                .iter()
                .find(|(c, _)| *c == command)
                .map(|(_, v)| v.clone())
                .ok_or(ClientError::Timeout)
        }
    }

    fn allow_args(preset: &str) -> Value {
        json!({"preset": preset, "agent_group_id": "ag-1"})
    }

    #[tokio::test]
    async fn allow_merges_preset_into_existing_list() {
        let t = KeyedTransport::new(vec![
            (
                "groups.config.get",
                json!({"agent_group_id": "ag-1", "egress_allow": ["db.local:5432"]}),
            ),
            (
                "groups.config.set-egress-allow",
                json!({"egress_allow": []}),
            ),
        ]);
        let out = run_egress_allow_preset(
            &allow_args("mongodb"),
            &t,
            Caller::Host,
            true,
            Palette::plain(),
        )
        .await;
        assert!(out.stderr.is_empty(), "{}", out.stderr);
        let sets = t.calls_for("groups.config.set-egress-allow");
        assert_eq!(sets.len(), 1);
        // Existing entry first (order preserved), preset entries appended.
        assert_eq!(
            sets[0]["allow"],
            json!([
                "db.local:5432",
                "fastdl.mongodb.org:443",
                "downloads.mongodb.com:443"
            ])
        );
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        assert_eq!(v["preset"], "mongodb");
        assert_eq!(v["added"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn allow_missing_config_row_reads_as_empty_list() {
        // groups.config.get returns null for an unconfigured group; the
        // merged list is exactly the preset.
        let t = KeyedTransport::new(vec![
            ("groups.config.get", Value::Null),
            (
                "groups.config.set-egress-allow",
                json!({"egress_allow": []}),
            ),
        ]);
        let out = run_egress_allow_preset(
            &allow_args("mongodb"),
            &t,
            Caller::Host,
            true,
            Palette::plain(),
        )
        .await;
        assert!(out.stderr.is_empty(), "{}", out.stderr);
        let sets = t.calls_for("groups.config.set-egress-allow");
        assert_eq!(
            sets[0]["allow"],
            json!(["fastdl.mongodb.org:443", "downloads.mongodb.com:443"])
        );
    }

    #[tokio::test]
    async fn allow_is_idempotent_and_skips_redundant_write() {
        let t = KeyedTransport::new(vec![(
            "groups.config.get",
            json!({
                "egress_allow": ["fastdl.mongodb.org:443", "downloads.mongodb.com:443"],
            }),
        )]);
        let out = run_egress_allow_preset(
            &allow_args("mongodb"),
            &t,
            Caller::Host,
            false,
            Palette::plain(),
        )
        .await;
        assert!(out.stderr.is_empty(), "{}", out.stderr);
        // No mutation call at all — nothing to add.
        assert!(t.calls_for("groups.config.set-egress-allow").is_empty());
        assert!(out.stdout.contains("no change"), "{}", out.stdout);
    }

    #[tokio::test]
    async fn allow_unknown_preset_fails_and_names_known_ones() {
        let t = KeyedTransport::new(vec![]);
        let out = run_egress_allow_preset(
            &allow_args("nope"),
            &t,
            Caller::Host,
            false,
            Palette::plain(),
        )
        .await;
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("unknown preset `nope`"));
        assert!(out.stderr.contains("mongodb"));
        // Never reached the transport.
        assert!(t.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allow_surfaces_get_failure() {
        // No canned responses: groups.config.get errors (Timeout).
        let t = KeyedTransport::new(vec![]);
        let out = run_egress_allow_preset(
            &allow_args("mongodb"),
            &t,
            Caller::Host,
            false,
            Palette::plain(),
        )
        .await;
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("groups.config.get failed"));
    }
}
