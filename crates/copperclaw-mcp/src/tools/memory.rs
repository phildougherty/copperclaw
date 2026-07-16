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

pub mod memory_save {
    //! `memory_save`: write a fact into this group's persistent memory so it
    //! survives across sessions. The write half of `memory_search` /
    //! `memory_get`.
    //!
    //! SECURITY: the agent CANNOT request a provenance. The side-effecting
    //! context decides it from the current turn's taint state (see
    //! [`crate::context::resolve_save_provenance`]): a turn tainted by
    //! untrusted-provenance content (e.g. a `web_fetch` body or an untrusted
    //! memory hit) is forced to write `untrusted`, so nothing lets an untrusted
    //! turn launder external content into trusted memory. This pure handler
    //! only validates + size-caps; the taint decision, the per-session rate
    //! cap, and the actual store write all live in the context impl.

    use crate::context::{MemorySaveSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};

    /// Max length (in `char`s) of a memory key.
    pub const MAX_KEY_LEN: usize = 256;
    /// Max size (in UTF-8 bytes) of a memory body. Keeps a confused or hostile
    /// model from dumping a whole document (or a prompt-injection payload) into
    /// the store.
    pub const MAX_BODY_BYTES: usize = 8 * 1024;
    /// Max length (in `char`s) of the optional source label.
    pub const MAX_SOURCE_LEN: usize = 256;
    /// Per-session ceiling on the number of `memory_save` writes. Enforced by
    /// the context impl (which owns the counter); exported here so the impl and
    /// the tests share one source of truth.
    pub const MAX_SAVES_PER_SESSION: usize = 100;

    pub fn schema() -> Tool {
        make_tool(
            "memory_save",
            "Save a fact into this agent group's persistent memory so you can recall it in a later session (the write side of memory_search/memory_get). Upserts under `key`; a later memory_search or memory_get returns it. Entries you save are recorded as 'trusted' — EXCEPT when the current turn has ingested untrusted external content (e.g. a web_fetch or an untrusted memory hit), in which case the save is honestly downgraded to 'untrusted' provenance (or refused) so external content is never laundered into trusted memory. Body is size-capped; there is a per-session write cap.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["key", "body"],
                "properties": {
                    "key": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_KEY_LEN,
                        "description": "Logical key to store under. Reusing a key overwrites that entry."
                    },
                    "body": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_BODY_BYTES,
                        "description": "The fact to remember. Keep it concise; capped at 8 KiB."
                    },
                    "source": {
                        "type": "string",
                        "maxLength": MAX_SOURCE_LEN,
                        "description": "Optional short note about where this fact came from."
                    }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let spec: MemorySaveSpec = parse_args(arguments)?;
        let key = spec.key.trim();
        if key.is_empty() {
            return Err(ToolError::Validation("`key` must be non-empty".into()));
        }
        if key.chars().count() > MAX_KEY_LEN {
            return Err(ToolError::Validation(format!(
                "`key` too long ({} chars); max is {MAX_KEY_LEN}",
                key.chars().count()
            )));
        }
        if spec.body.trim().is_empty() {
            return Err(ToolError::Validation("`body` must be non-empty".into()));
        }
        // Size cap on the raw UTF-8 body — the load-bearing guard against a
        // model dumping a document (or an injection payload) into the store.
        if spec.body.len() > MAX_BODY_BYTES {
            return Err(ToolError::Validation(format!(
                "`body` too large ({} bytes); max is {MAX_BODY_BYTES} bytes",
                spec.body.len()
            )));
        }
        if let Some(src) = spec.source.as_ref() {
            let n = src.chars().count();
            if n > MAX_SOURCE_LEN {
                return Err(ToolError::Validation(format!(
                    "`source` too long ({n} chars); max is {MAX_SOURCE_LEN}"
                )));
            }
        }
        // Normalize the key (trimmed); the context decides provenance + rate
        // cap + performs the store write.
        let normalized = MemorySaveSpec {
            key: key.to_string(),
            body: spec.body,
            source: spec.source.filter(|s| !s.trim().is_empty()),
        };
        let outcome = ctx.memory_save(normalized).await?;
        Ok(success_json(&outcome))
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
        assert_eq!(
            super::memory_save::entry().tool.name.as_ref(),
            "memory_save"
        );
    }

    // ----- memory_save (write side) -----

    use crate::context::{MemorySaveOutcome, MemorySaveSpec, resolve_save_provenance};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stateful context whose `memory_save` honors the taint→provenance rule
    /// (via [`resolve_save_provenance`]) and the per-session rate cap exactly
    /// as the runner impl must; `memory_search` / `memory_get` read the same
    /// in-memory store back. This mock IS the reference for the contract the
    /// runner's `RunnerToolCtx` implements, so write→retrieve is exercised end
    /// to end without the runner.
    #[derive(Default)]
    struct SaveMock {
        store: Mutex<Vec<MemoryHitView>>,
        tainted: bool,
        saves: AtomicUsize,
    }

    #[async_trait]
    impl ToolContext for SaveMock {
        async fn emit_outbound(
            &self,
            _e: crate::context::OutboundToolEffect,
        ) -> Result<crate::context::ToolEffectAck, ToolError> {
            Ok(crate::context::ToolEffectAck::Accepted)
        }
        async fn list_tasks(&self) -> Result<Vec<crate::context::TaskSummary>, ToolError> {
            Ok(Vec::new())
        }
        fn is_context_tainted(&self) -> bool {
            self.tainted
        }
        async fn memory_save(&self, spec: MemorySaveSpec) -> Result<MemorySaveOutcome, ToolError> {
            // Per-session rate cap (the impl owns the counter).
            if self.saves.fetch_add(1, Ordering::SeqCst)
                >= super::memory_save::MAX_SAVES_PER_SESSION
            {
                return Err(ToolError::Validation(
                    "memory_save rate cap reached for this session".into(),
                ));
            }
            // Provenance is decided HERE from the turn's taint — never from the
            // caller — so an untrusted turn cannot launder into trusted memory.
            let (prov, downgraded) = resolve_save_provenance(self.is_context_tainted());
            let mut store = self.store.lock().unwrap();
            store.retain(|h| h.key != spec.key);
            store.push(MemoryHitView {
                key: spec.key.clone(),
                body: spec.body,
                provenance: prov.to_string(),
                source: spec.source,
                score: None,
                updated_at: "2026-01-01T00:00:00Z".into(),
            });
            Ok(MemorySaveOutcome {
                key: spec.key,
                provenance: prov.to_string(),
                downgraded,
            })
        }
        async fn memory_search(
            &self,
            spec: MemorySearchSpec,
        ) -> Result<Vec<MemoryHitView>, ToolError> {
            Ok(self
                .store
                .lock()
                .unwrap()
                .iter()
                .filter(|h| h.body.contains(&spec.query) || h.key.contains(&spec.query))
                .take(spec.limit.unwrap_or(5))
                .cloned()
                .collect())
        }
        async fn memory_get(&self, key: &str) -> Result<Option<MemoryHitView>, ToolError> {
            Ok(self
                .store
                .lock()
                .unwrap()
                .iter()
                .find(|h| h.key == key)
                .cloned())
        }
    }

    #[tokio::test]
    async fn save_then_search_returns_trusted_entry() {
        let ctx = SaveMock::default();
        let saved = super::memory_save::handle(
            args(serde_json::json!({"key": "fav_editor", "body": "phil uses helix"})),
            &ctx,
        )
        .await
        .unwrap();
        let out = text(&saved);
        assert!(out.contains("\"provenance\": \"trusted\""), "got: {out}");
        assert!(out.contains("\"downgraded\": false"), "got: {out}");

        // A later memory_search returns the freshly-saved entry as trusted.
        let found = super::memory_search::handle(args(serde_json::json!({"query": "helix"})), &ctx)
            .await
            .unwrap();
        let body = text(&found);
        assert!(body.contains("phil uses helix"), "got: {body}");
        assert!(body.contains("trusted"), "got: {body}");
    }

    #[tokio::test]
    async fn tainted_turn_downgrades_to_untrusted() {
        // A turn tainted by untrusted-provenance content cannot write trusted
        // memory: the entry is forced to untrusted (no laundering).
        let ctx = SaveMock {
            tainted: true,
            ..Default::default()
        };
        let saved = super::memory_save::handle(
            args(serde_json::json!({"key": "scraped", "body": "value from a web page"})),
            &ctx,
        )
        .await
        .unwrap();
        let out = text(&saved);
        assert!(out.contains("\"provenance\": \"untrusted\""), "got: {out}");
        assert!(out.contains("\"downgraded\": true"), "got: {out}");

        // And it reads back as untrusted, never trusted.
        let got = super::memory_get::handle(args(serde_json::json!({"key": "scraped"})), &ctx)
            .await
            .unwrap();
        let body = text(&got);
        assert!(body.contains("untrusted"), "got: {body}");
        assert!(!body.contains("\"provenance\": \"trusted\""), "got: {body}");
    }

    #[tokio::test]
    async fn cap_rejects_oversized_body() {
        let ctx = SaveMock::default();
        let big = "x".repeat(super::memory_save::MAX_BODY_BYTES + 1);
        let err =
            super::memory_save::handle(args(serde_json::json!({"key": "k", "body": big})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)), "got: {err:?}");
        // Rejected before the context write — nothing landed in the store.
        assert!(ctx.store.lock().unwrap().is_empty());
        assert_eq!(ctx.saves.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_empty_key_and_body() {
        let ctx = SaveMock::default();
        let e1 =
            super::memory_save::handle(args(serde_json::json!({"key": "  ", "body": "x"})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(e1, ToolError::Validation(_)));
        let e2 =
            super::memory_save::handle(args(serde_json::json!({"key": "k", "body": "   "})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(e2, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn per_session_rate_cap_rejects_excess_writes() {
        let ctx = SaveMock::default();
        for i in 0..super::memory_save::MAX_SAVES_PER_SESSION {
            super::memory_save::handle(
                args(serde_json::json!({"key": format!("k{i}"), "body": "b"})),
                &ctx,
            )
            .await
            .unwrap();
        }
        let err = super::memory_save::handle(
            args(serde_json::json!({"key": "one_too_many", "body": "b"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn save_errors_on_context_without_memory() {
        // The default MockToolContext has no memory store wired — the trait
        // default surfaces a Context error rather than panicking.
        let ctx = MockToolContext::new();
        let err =
            super::memory_save::handle(args(serde_json::json!({"key": "k", "body": "b"})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(err, ToolError::Context(_)));
    }
}
