//! Per-server tool include/exclude filtering for external MCP servers.
//!
//! A group's external MCP server config (one entry in
//! `container_configs.mcp_servers`) may declare an allow-list and/or a
//! deny-list of tool names. This module is the **single enforcement point**
//! that decides whether a given remote tool is permitted — so a denied MCP
//! tool never reaches the model and a denied call is refused even if the
//! model somehow names it anyway.
//!
//! ## Precedence (default-safe)
//!
//! 1. **Deny wins.** A name on the deny-list is rejected, full stop — even
//!    if it also appears on the allow-list. Deny is the hard floor.
//! 2. **Allow-list is a positive gate.** When an allow-list is present
//!    (non-empty), a name must be on it to pass. An empty allow-list means
//!    "no allow-list configured" — every non-denied tool passes (unchanged
//!    default behaviour for servers that never declared a filter).
//!
//! Matching is exact and case-sensitive: MCP tool names are stable
//! identifiers, not human prose, so fuzzy matching would only create
//! confusing allow/deny surprises.
//!
//! The filter is constructed from the server's JSON entry via
//! [`ToolFilter::from_server_entry`], so the host (`mcp.add` /
//! `add_mcp_server` approval) and the runner (which spins up the client and
//! lists tools) agree on exactly the same parsing.

use std::collections::BTreeSet;

use crate::client::RemoteTool;

/// JSON key on an `mcp_servers` entry holding the positive allow-list of
/// tool names. Absent or empty ⇒ no allow-list (every non-denied tool
/// passes). Accepts the `allowed_tools` spelling.
pub const ALLOWED_TOOLS_KEY: &str = "allowed_tools";

/// JSON key on an `mcp_servers` entry holding the deny-list of tool names.
/// A name here is always rejected. Accepts the `denied_tools` spelling.
pub const DENIED_TOOLS_KEY: &str = "denied_tools";

/// A parsed include/exclude filter for one external MCP server.
///
/// Cheap to clone; built once per server when the runner connects and reused
/// for the list-tools advertisement *and* every subsequent `call_tool`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolFilter {
    /// Positive allow-list names. Only meaningful when [`Self::allow_declared`]
    /// is true.
    allow: BTreeSet<String>,
    /// Whether an allow-list was declared on the server entry. A declared
    /// allow-list is a positive gate even when empty (an empty allow-list is
    /// a deliberate "permit nothing" kill switch); an *undeclared* allow-list
    /// leaves the server open (the historical default).
    allow_declared: bool,
    /// Deny-list. A name here is always rejected.
    deny: BTreeSet<String>,
}

/// Why a tool was rejected by a [`ToolFilter`]. Carried so callers can log /
/// surface a precise reason rather than a bare boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterRejection {
    /// The tool name is on the server's deny-list.
    Denied,
    /// The server declares an allow-list and the tool is not on it.
    NotAllowed,
}

impl FilterRejection {
    /// Stable token for logs / JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FilterRejection::Denied => "denied",
            FilterRejection::NotAllowed => "not-allowed",
        }
    }
}

impl ToolFilter {
    /// Construct from explicit allow / deny name lists. The allow-list is
    /// treated as *declared* (a positive gate) — pass an empty iterator and
    /// you get a "permit nothing" filter. Use [`ToolFilter::deny_only`] when
    /// you only want a deny-list.
    #[must_use]
    pub fn new<I, J>(allow: I, deny: J) -> Self
    where
        I: IntoIterator<Item = String>,
        J: IntoIterator<Item = String>,
    {
        Self {
            allow: allow.into_iter().collect(),
            allow_declared: true,
            deny: deny.into_iter().collect(),
        }
    }

    /// Construct a deny-only filter (no allow-list gate; every non-denied
    /// tool passes).
    #[must_use]
    pub fn deny_only<J>(deny: J) -> Self
    where
        J: IntoIterator<Item = String>,
    {
        Self {
            allow: BTreeSet::new(),
            allow_declared: false,
            deny: deny.into_iter().collect(),
        }
    }

    /// Parse the filter out of a single `mcp_servers` entry.
    ///
    /// Reads the `allowed_tools` / `denied_tools` keys when present. A
    /// non-array value (or a non-string element) for either key is ignored
    /// for that key — a malformed filter must never silently widen access,
    /// and ignoring a malformed *allow*-list would do exactly that, so we
    /// treat a present-but-malformed `allowed_tools` as an empty allow-list
    /// (which blocks every tool) rather than as "no filter". A malformed
    /// `denied_tools` is treated as empty (it can only ever *narrow*, so a
    /// parse miss there is fail-open by nature and we keep it simple).
    ///
    /// An entry with neither key yields the default (open) filter, preserving
    /// the historical behaviour for servers added before filtering existed.
    #[must_use]
    pub fn from_server_entry(entry: &serde_json::Value) -> Self {
        // `allowed_tools` present (even if it parsed to empty because every
        // element was malformed, or because the operator wrote `[]`) is a
        // deliberate restriction. We distinguish "key absent" from "key
        // present" so an explicit empty list means "allow nothing".
        let allow_declared = entry.get(ALLOWED_TOOLS_KEY).is_some();
        Self {
            allow: parse_name_list(entry.get(ALLOWED_TOOLS_KEY)),
            allow_declared,
            deny: parse_name_list(entry.get(DENIED_TOOLS_KEY)),
        }
    }

    /// Whether this filter actually restricts anything. A default filter
    /// (no allow-list declared, no deny entries) is a no-op and callers can
    /// skip the per-tool check entirely.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.allow_declared && self.deny.is_empty()
    }

    /// Decide whether `tool_name` is permitted.
    ///
    /// Returns `Ok(())` when the tool passes, or `Err(reason)` when it is
    /// rejected. This is the function both the list-tools advertisement and
    /// the per-call gate must funnel through.
    pub fn check(&self, tool_name: &str) -> Result<(), FilterRejection> {
        // Deny is the hard floor: a denied name is rejected even if it is
        // also allow-listed.
        if self.deny.contains(tool_name) {
            return Err(FilterRejection::Denied);
        }
        // A declared allow-list is a positive gate. An empty-but-declared
        // allow-list rejects everything (a deliberate kill switch).
        if self.allow_declared && !self.allow.contains(tool_name) {
            return Err(FilterRejection::NotAllowed);
        }
        Ok(())
    }

    /// Convenience predicate over [`Self::check`].
    #[must_use]
    pub fn permits(&self, tool_name: &str) -> bool {
        self.check(tool_name).is_ok()
    }

    /// Filter a list of advertised remote tools down to the permitted set.
    ///
    /// This is what the runner applies to the `list_tools` result before the
    /// tool descriptors ever reach the model — a denied tool is simply not
    /// advertised. Order is preserved.
    #[must_use]
    pub fn apply(&self, tools: Vec<RemoteTool>) -> Vec<RemoteTool> {
        tools
            .into_iter()
            .filter(|t| self.permits(&t.name))
            .collect()
    }

    /// The number of allow-list entries (for inspection / tests).
    #[must_use]
    pub fn allow_len(&self) -> usize {
        self.allow.len()
    }

    /// The number of deny-list entries (for inspection / tests).
    #[must_use]
    pub fn deny_len(&self) -> usize {
        self.deny.len()
    }
}

/// Parse a JSON value expected to be an array of strings into a name set.
/// Non-array values and non-string elements are skipped (the malformed-list
/// handling is documented on [`ToolFilter::from_server_entry`]).
fn parse_name_list(value: Option<&serde_json::Value>) -> BTreeSet<String> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return BTreeSet::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> RemoteTool {
        RemoteTool {
            name: name.to_owned(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn default_filter_is_open() {
        let f = ToolFilter::default();
        assert!(f.permits("anything"));
        assert!(f.permits("read"));
        assert!(f.check("x").is_ok());
    }

    #[test]
    fn entry_without_keys_is_open() {
        let entry = serde_json::json!({"command": "npx", "args": []});
        let f = ToolFilter::from_server_entry(&entry);
        assert!(f.permits("read"));
        assert!(f.permits("write"));
        assert_eq!(f.allow_len(), 0);
        assert_eq!(f.deny_len(), 0);
    }

    #[test]
    fn deny_list_blocks_named_tool() {
        let entry = serde_json::json!({"denied_tools": ["delete_repo", "force_push"]});
        let f = ToolFilter::from_server_entry(&entry);
        assert!(f.permits("read"));
        assert!(!f.permits("delete_repo"));
        assert_eq!(f.check("delete_repo"), Err(FilterRejection::Denied));
        assert_eq!(f.check("force_push"), Err(FilterRejection::Denied));
    }

    #[test]
    fn allow_list_is_positive_gate() {
        let entry = serde_json::json!({"allowed_tools": ["read", "search"]});
        let f = ToolFilter::from_server_entry(&entry);
        assert!(f.permits("read"));
        assert!(f.permits("search"));
        // Not on the allow-list ⇒ rejected.
        assert_eq!(f.check("write"), Err(FilterRejection::NotAllowed));
        assert_eq!(f.check("delete"), Err(FilterRejection::NotAllowed));
    }

    #[test]
    fn deny_wins_over_allow() {
        // A name on both lists is denied — deny is the hard floor.
        let entry = serde_json::json!({
            "allowed_tools": ["read", "danger"],
            "denied_tools": ["danger"],
        });
        let f = ToolFilter::from_server_entry(&entry);
        assert!(f.permits("read"));
        assert_eq!(f.check("danger"), Err(FilterRejection::Denied));
    }

    #[test]
    fn apply_filters_advertised_tools() {
        let entry = serde_json::json!({"denied_tools": ["delete_repo"]});
        let f = ToolFilter::from_server_entry(&entry);
        let tools = vec![tool("read"), tool("delete_repo"), tool("search")];
        let kept = f.apply(tools);
        let names: Vec<&str> = kept.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["read", "search"]);
    }

    #[test]
    fn apply_with_allow_list_keeps_only_allowed() {
        let entry = serde_json::json!({"allowed_tools": ["read"]});
        let f = ToolFilter::from_server_entry(&entry);
        let tools = vec![tool("read"), tool("write"), tool("delete")];
        let kept = f.apply(tools);
        let names: Vec<&str> = kept.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["read"]);
    }

    #[test]
    fn apply_preserves_order() {
        let entry = serde_json::json!({"denied_tools": ["b"]});
        let f = ToolFilter::from_server_entry(&entry);
        let tools = vec![tool("c"), tool("a"), tool("b"), tool("d")];
        let kept = f.apply(tools);
        let names: Vec<&str> = kept.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["c", "a", "d"]);
    }

    #[test]
    fn explicit_empty_allow_list_blocks_everything() {
        // An operator who writes `allowed_tools: []` means "this server is
        // present but no tool is permitted" — a deliberate kill switch.
        let entry = serde_json::json!({"allowed_tools": []});
        let f = ToolFilter::from_server_entry(&entry);
        assert_eq!(f.check("read"), Err(FilterRejection::NotAllowed));
        assert!(!f.permits("anything"));
    }

    #[test]
    fn malformed_allow_list_fails_closed() {
        // A non-array `allowed_tools` is malformed; treating it as "no
        // filter" would silently widen access. We fail closed: present key,
        // empty parsed list ⇒ block everything.
        let entry = serde_json::json!({"allowed_tools": "read"});
        let f = ToolFilter::from_server_entry(&entry);
        assert_eq!(f.check("read"), Err(FilterRejection::NotAllowed));
    }

    #[test]
    fn malformed_deny_list_is_empty() {
        let entry = serde_json::json!({"denied_tools": 42});
        let f = ToolFilter::from_server_entry(&entry);
        // Deny couldn't parse ⇒ no denies; open by default.
        assert!(f.permits("read"));
    }

    #[test]
    fn non_string_elements_are_skipped() {
        let entry = serde_json::json!({"denied_tools": ["ok", 7, null, "fine"]});
        let f = ToolFilter::from_server_entry(&entry);
        assert!(!f.permits("ok"));
        assert!(!f.permits("fine"));
        assert_eq!(f.deny_len(), 2);
    }

    #[test]
    fn new_constructor_round_trips() {
        let f = ToolFilter::new(["a".to_owned()], ["b".to_owned()]);
        assert!(f.permits("a"));
        assert!(!f.permits("b"));
        assert_eq!(f.check("c"), Err(FilterRejection::NotAllowed));
    }

    #[test]
    fn rejection_tokens_are_stable() {
        assert_eq!(FilterRejection::Denied.as_str(), "denied");
        assert_eq!(FilterRejection::NotAllowed.as_str(), "not-allowed");
    }

    #[test]
    fn deny_only_constructor_leaves_server_open() {
        let f = ToolFilter::deny_only(["bad".to_owned()]);
        assert!(!f.permits("bad"));
        assert!(f.permits("good"));
        assert!(!f.is_open()); // a deny entry means it's not a no-op
    }

    #[test]
    fn is_open_is_true_only_for_default() {
        assert!(ToolFilter::default().is_open());
        assert!(ToolFilter::from_server_entry(&serde_json::json!({})).is_open());
        assert!(
            !ToolFilter::from_server_entry(&serde_json::json!({"allowed_tools": []})).is_open()
        );
        assert!(
            !ToolFilter::from_server_entry(&serde_json::json!({"denied_tools": ["x"]})).is_open()
        );
    }

    #[test]
    fn case_sensitive_matching() {
        let entry = serde_json::json!({"denied_tools": ["Delete"]});
        let f = ToolFilter::from_server_entry(&entry);
        // Exact case only: `delete` is not `Delete`.
        assert!(f.permits("delete"));
        assert!(!f.permits("Delete"));
    }
}
