//! `memory_search` / `memory_get`: query the agent group's searchable memory
//! store (M16 Phase 3).
//!
//! The store is a per-group `SQLite` DB (`copperclaw_db::memory::MemoryStore`)
//! with hybrid retrieval — FTS5 full-text plus pure-Rust cosine over stored
//! embedding blobs. These tools are read-only over that store; the writing
//! side (and embedding generation) lives in the runner / host. Both tools
//! delegate to the [`ToolContext`], which the runner implements against the
//! bind-mounted `memory.db` for the session's group.
//!
//! PROVENANCE: entries carry a `trusted` / `untrusted` tag. The runner's
//! `ToolContext` impl marks the current turn tainted whenever a returned hit is
//! untrusted, so the coarse approval gate blocks credentialed external actions
//! until fresh approval. These tool handlers stay pure — the taint side-effect
//! is the context's responsibility (the same shape as every other effect).

pub mod memory_search {
    //! `memory_search`: hybrid (FTS5 + vector) search of group memory.

    use crate::context::{MemorySearchSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};

    /// Ceiling on `limit` so a confused model can't pull the whole store.
    const MAX_LIMIT: usize = 25;

    pub fn schema() -> Tool {
        make_tool(
            "memory_search",
            "Search this agent group's persistent memory for entries relevant to a query. Hybrid retrieval: full-text plus vector similarity over stored notes. Returns ranked hits with their key, body, provenance ('trusted' = you/the operator wrote it; 'untrusted' = lifted from an external source like a web fetch), and source. Read-only.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Free-text search query."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": "Maximum hits to return (default 5, capped at 25)."
                    }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let mut spec: MemorySearchSpec = parse_args(arguments)?;
        if spec.query.trim().is_empty() {
            return Err(ToolError::Validation("`query` must be non-empty".into()));
        }
        // Clamp the limit: default 5, hard ceiling MAX_LIMIT.
        spec.limit = Some(spec.limit.unwrap_or(5).clamp(1, MAX_LIMIT));
        let hits = ctx.memory_search(spec).await?;
        Ok(success_json(&hits))
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }

    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod memory_get {
    //! `memory_get`: fetch one memory entry by its exact key.

    use crate::context::ToolContext;
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        key: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "memory_get",
            "Fetch one entry from this agent group's persistent memory by its exact key. Returns the entry's body, provenance, and source, or an explicit not-found result. Read-only.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["key"],
                "properties": {
                    "key": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The exact logical key of the memory entry."
                    }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let key = input.key.trim();
        if key.is_empty() {
            return Err(ToolError::Validation("`key` must be non-empty".into()));
        }
        match ctx.memory_get(key).await? {
            Some(hit) => Ok(success_json(&hit)),
            None => Ok(success_json(&serde_json::json!({
                "found": false,
                "key": key,
            }))),
        }
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }

    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::context::{MemoryHitView, MemorySearchSpec, MockToolContext, ToolContext};
    use crate::error::ToolError;
    use async_trait::async_trait;
    use rmcp::model::{CallToolResult, JsonObject};
    use std::sync::Mutex;

    /// A context with a tiny in-memory map backing `memory_search` /
    /// `memory_get`, so the tool handlers can be exercised end to end without
    /// the runner. Records the search specs it saw.
    #[derive(Default)]
    struct MemoryMock {
        hits: Vec<MemoryHitView>,
        searches: Mutex<Vec<MemorySearchSpec>>,
    }

    #[async_trait]
    impl ToolContext for MemoryMock {
        async fn emit_outbound(
            &self,
            _e: crate::context::OutboundToolEffect,
        ) -> Result<crate::context::ToolEffectAck, ToolError> {
            Ok(crate::context::ToolEffectAck::Accepted)
        }
        async fn list_tasks(&self) -> Result<Vec<crate::context::TaskSummary>, ToolError> {
            Ok(Vec::new())
        }
        async fn memory_search(
            &self,
            spec: MemorySearchSpec,
        ) -> Result<Vec<MemoryHitView>, ToolError> {
            self.searches.lock().unwrap().push(spec.clone());
            Ok(self
                .hits
                .iter()
                .filter(|h| h.body.contains(&spec.query) || h.key.contains(&spec.query))
                .take(spec.limit.unwrap_or(5))
                .cloned()
                .collect())
        }
        async fn memory_get(&self, key: &str) -> Result<Option<MemoryHitView>, ToolError> {
            Ok(self.hits.iter().find(|h| h.key == key).cloned())
        }
    }

    fn hit(key: &str, body: &str, prov: &str) -> MemoryHitView {
        MemoryHitView {
            key: key.into(),
            body: body.into(),
            provenance: prov.into(),
            source: None,
            score: Some(1.0),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn args(v: serde_json::Value) -> Option<JsonObject> {
        match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    fn text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| {
                let raw = serde_json::to_value(c).ok()?;
                raw.get("text")?.as_str().map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn search_returns_matching_hits() {
        let ctx = MemoryMock {
            hits: vec![
                hit("runbook", "telegram deploy steps", "trusted"),
                hit("groceries", "milk eggs", "trusted"),
            ],
            ..Default::default()
        };
        let r = super::memory_search::handle(args(serde_json::json!({"query": "telegram"})), &ctx)
            .await
            .unwrap();
        let body = text(&r);
        assert!(body.contains("runbook"), "got: {body}");
        assert!(!body.contains("groceries"), "got: {body}");
    }

    #[tokio::test]
    async fn search_clamps_limit_to_ceiling() {
        let ctx = MemoryMock::default();
        super::memory_search::handle(args(serde_json::json!({"query": "x", "limit": 1000})), &ctx)
            .await
            .unwrap();
        let seen = ctx.searches.lock().unwrap();
        assert_eq!(seen[0].limit, Some(25), "limit must clamp to MAX_LIMIT");
    }

    #[tokio::test]
    async fn search_defaults_limit_when_absent() {
        let ctx = MemoryMock::default();
        super::memory_search::handle(args(serde_json::json!({"query": "x"})), &ctx)
            .await
            .unwrap();
        assert_eq!(ctx.searches.lock().unwrap()[0].limit, Some(5));
    }

    #[tokio::test]
    async fn search_rejects_empty_query() {
        let ctx = MemoryMock::default();
        let err = super::memory_search::handle(args(serde_json::json!({"query": "   "})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn get_returns_entry_or_not_found() {
        let ctx = MemoryMock {
            hits: vec![hit("k", "the body", "untrusted")],
            ..Default::default()
        };
        let found = super::memory_get::handle(args(serde_json::json!({"key": "k"})), &ctx)
            .await
            .unwrap();
        let body = text(&found);
        assert!(body.contains("the body"));
        assert!(body.contains("untrusted"));

        let missing = super::memory_get::handle(args(serde_json::json!({"key": "nope"})), &ctx)
            .await
            .unwrap();
        let body = text(&missing);
        assert!(body.contains("\"found\": false"), "got: {body}");
    }

    #[tokio::test]
    async fn tools_error_on_context_without_memory() {
        // The default MockToolContext has no memory store wired — the trait
        // default surfaces a Context error rather than panicking.
        let ctx = MockToolContext::new();
        let err = super::memory_search::handle(args(serde_json::json!({"query": "x"})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Context(_)));
        let err = super::memory_get::handle(args(serde_json::json!({"key": "x"})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Context(_)));
    }

    #[test]
    fn entries_have_expected_names() {
        assert_eq!(
            super::memory_search::entry().tool.name.as_ref(),
            "memory_search"
        );
        assert_eq!(super::memory_get::entry().tool.name.as_ref(), "memory_get");
    }
}
