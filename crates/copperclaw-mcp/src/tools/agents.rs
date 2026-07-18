//! Agent-management tools: `create_agent`, `delegate`.

pub mod create_agent {
    //! `create_agent`: ask the host to spawn a fresh sibling agent.

    use crate::context::{CreateAgentSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        name: String,
        instructions: String,
        #[serde(default)]
        channel: Option<String>,
    }

    pub fn schema() -> Tool {
        make_tool(
            "create_agent",
            "Request the host to spawn a sibling agent (own container, full tool access, \
             unbounded — it reports back into your messages_in). The sibling has its OWN \
             fresh workspace at /data. If the project you're CURRENTLY in (your shell's \
             working dir) is a GIT REPO, the sibling also gets a WRITABLE git worktree of \
             THAT repo at /workspace on its own branch (sib/<id>): it can edit AND commit \
             there, isolated from your files. Commits go into the shared object store, so \
             after it finishes you review and merge its branch from inside that project \
             (`git diff main..sib/<id>`, `git merge sib/<id>`); your checked-out files are \
             never touched until you merge. So `cd` into the project (and `git init` it if \
             new) before spawning builders. If you're NOT in a git repo, your workspace is \
             instead mounted READ-ONLY at /parent (review/audit only). ALWAYS point the \
             sibling at its workspace in `instructions` — e.g. 'implement X under \
             /workspace and commit' (git repo) or 'review the code under /parent' \
             (read-only). Use `create_agent` for SUBSTANTIVE PARALLEL work over your \
             codebase or independent research; for a QUICK in-process lookup that shares \
             your live workspace directly, prefer `explore`.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "instructions"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "instructions": { "type": "string", "minLength": 1 },
                    "channel": { "type": ["string", "null"] }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.name.trim().is_empty() {
            return Err(ToolError::Validation("`name` must be non-empty".into()));
        }
        if input.instructions.trim().is_empty() {
            return Err(ToolError::Validation(
                "`instructions` must be non-empty".into(),
            ));
        }
        if let Some(c) = input.channel.as_ref() {
            if c.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`channel`, when present, must be non-empty".into(),
                ));
            }
        }
        let spec = CreateAgentSpec {
            name: input.name,
            instructions: input.instructions,
            channel: input.channel,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::CreateAgent(spec))
            .await?;
        Ok(ack_to_result(&ack))
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

pub mod delegate {
    //! `delegate`: spawn a write-capable, parent-only build worker.
    //!
    //! `delegate` is the MIDDLE TIER between the read-only in-process
    //! `explore` subagent and a full persistent `create_agent` sibling.
    //! It reuses the exact `create_agent` container-spawn + git-worktree
    //! mechanics (own session, writable `/workspace` on branch
    //! `sib/<id>`), so a delegate can edit AND commit in isolation and
    //! its writes land in the parent repo via the branch-merge path. It
    //! differs from `create_agent` in being CONTAINED: a delegate is
    //! never wired into a channel and cannot post into the user's chat —
    //! it reports ONLY back to the spawning parent. Fan a few out (call
    //! `delegate` several times) and each gets its own isolated worktree.

    use crate::context::{DelegateSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        name: String,
        instructions: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "delegate",
            "Spawn a write-capable BUILD WORKER — the middle tier between `explore` \
             (read-only, in-process) and `create_agent` (a full, user-facing sibling). \
             Like `create_agent`, a delegate runs in its OWN container and, if the project \
             you're CURRENTLY in (your shell's working dir) is a GIT REPO, gets a WRITABLE \
             git worktree of THAT repo at /workspace on its own branch (sib/<id>): it can \
             edit AND commit there, isolated from your files and from any sibling delegates \
             (each delegate gets its OWN worktree). After it finishes you review and merge \
             its branch (`git diff main..sib/<id>`, `git merge sib/<id>`); your checked-out \
             files are never touched until you merge. UNLIKE `create_agent`, a delegate is \
             CONTAINED: it is NOT wired to any channel and CANNOT message the user's chat — \
             it reports ONLY back to you. Use it for SUBSTANTIVE PARALLEL BUILD work: `cd` \
             into the project (`git init` it if new), then call `delegate` once per \
             independent piece of work — always point each one at /workspace in \
             `instructions` (e.g. 'implement X under /workspace and commit'). For a QUICK \
             read-only lookup that shares your live workspace, prefer `explore`; for a \
             persistent user-facing sibling, use `create_agent`.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "instructions"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "instructions": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.name.trim().is_empty() {
            return Err(ToolError::Validation("`name` must be non-empty".into()));
        }
        if input.instructions.trim().is_empty() {
            return Err(ToolError::Validation(
                "`instructions` must be non-empty".into(),
            ));
        }
        let spec = DelegateSpec {
            name: input.name,
            instructions: input.instructions,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::Delegate(spec))
            .await?;
        Ok(ack_to_result(&ack))
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

pub mod delegate_batch {
    //! `delegate_batch`: spawn N `delegate` workers in parallel and BLOCK
    //! until all report, returning one aggregated result.
    //!
    //! This is the single-call parallel fan-out with a JOIN (M19 A1). Where
    //! plain `delegate` is fire-and-forget (each worker reports back
    //! asynchronously on a later turn), `delegate_batch` spawns a bounded
    //! set of workers — each in its OWN container with its OWN writable
    //! `sib/<id>` worktree, each contained (reports only to you, never to
    //! the user's chat, exactly like a single `delegate`) — and gathers
    //! their reports into ONE tool response before returning. A parent that
    //! wants to build three components in parallel and assemble them can now
    //! block on all three in a single turn.
    //!
    //! The spawn machinery is identical to `delegate` (same depth /
    //! permission caps, same isolation); the only new thing is the join.
    //! The runner's `ToolContext::run_delegate_batch` implements the join
    //! seam by block-polling the parent's inbound for each worker's spawn
    //! result and final report.
    //!
    //! M20 Q7: an OPTIONAL `contract` arg — a parent-authored shared brief
    //! (interfaces, file-ownership map, naming conventions) — is prepended
    //! VERBATIM to every worker's instructions, together with a directive
    //! telling the worker to persist it to `.copperclaw/CONTRACT.md` in its
    //! own `/workspace` before its first edit, so the brief survives the
    //! worker's own compaction (`mark_dirty_for_write`'s state-dir exemption,
    //! `verify_gate.rs`, already anticipates that file). An OPTIONAL
    //! `project` arg names the PARENT's own project directory (the one it
    //! `cd`'d into before delegating); when given and that project has a
    //! recorded `.copperclaw/verify`, the join marks it dirty via the
    //! existing Q2 verify-gate machinery (`verify_gate::mark_dirty`) — the
    //! parent cannot complete its integration todo without re-running every
    //! stage against the merged worker branches. Neither field's absence
    //! changes anything: a batch with no `contract` sends instructions
    //! byte-identical to before, and a batch with no `project` never touches
    //! the verify-gate state — full back-compat with pre-Q7 callers.

    use crate::context::{
        DEFAULT_DELEGATE_BATCH_TIMEOUT_SECS, DelegateBatchRequest, DelegateBatchWorker,
        MAX_DELEGATE_BATCH_TIMEOUT_SECS, MAX_DELEGATE_BATCH_WIDTH, ToolContext,
    };
    use crate::error::ToolError;
    use crate::tools::verify_gate;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        workers: Vec<InputWorker>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// M20 Q7: shared brief prepended verbatim to every worker's
        /// instructions and persisted by each worker as
        /// `.copperclaw/CONTRACT.md`.
        #[serde(default)]
        contract: Option<String>,
        /// M20 Q7: the PARENT's own project directory. When set and that
        /// project has a recorded `.copperclaw/verify`, the join marks it
        /// dirty so the merged union gets re-verified before delivery.
        #[serde(default)]
        project: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    struct InputWorker {
        name: String,
        instructions: String,
        /// M22 C5: worker role. Absent (or `build`) is a normal build
        /// worker — the pre-C5 shape. `review` makes the worker a REVIEWER:
        /// it drives the existing `code-review` skill over the changes and
        /// its verdict gates the merge (see [`review_instructions`] +
        /// [`parse_review_verdict`]).
        #[serde(default)]
        role: Option<WorkerRole>,
        /// M22 C5: for a `review` worker, the diff to review — embedded
        /// verbatim into the reviewer's instructions so it "receives the
        /// diff" directly (it may also inspect the sibling worker branches
        /// under `/workspace`). Ignored for a build worker.
        #[serde(default)]
        diff: Option<String>,
    }

    /// M22 C5: the role a `delegate_batch` worker plays.
    #[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
    #[serde(rename_all = "lowercase")]
    enum WorkerRole {
        /// A write-capable build worker (the default, pre-C5 behavior).
        #[default]
        Build,
        /// A reviewer that drives the `code-review` skill over the diff and
        /// whose findings gate the merge.
        Review,
    }

    /// M22 C5: sentinel prefixed onto a review worker's instructions so the
    /// runner-side join (`copperclaw-runner`'s `run::delegate_batch`) can
    /// recognize the reviewer among the fanned-out workers WITHOUT a schema
    /// change to [`crate::context::DelegateBatchWorker`] (which carries only
    /// `name` + `instructions`). Kept `pub` because that join imports it —
    /// together with [`parse_review_verdict`] — to gate the merge at the
    /// join layer too.
    pub const REVIEWER_ROLE_SENTINEL: &str = "[copperclaw:reviewer]";

    /// M22 C5: the machine-readable verdict line a review worker is directed
    /// to end its report with; a case-insensitive `pass`/`block` token
    /// follows it. Shared between the tool-surface gate here and the
    /// runner-side join so both layers agree on the protocol.
    pub const REVIEW_VERDICT_MARKER: &str = "REVIEW-VERDICT:";

    /// M22 C5: a review worker's verdict, parsed from its report text.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ReviewVerdict {
        /// Reviewer signed off — no blocking findings, safe to merge.
        Pass,
        /// Reviewer found blocking issues — the merge must NOT proceed.
        Block,
        /// The report carried no recognizable verdict marker. The gate
        /// treats this as blocking (fail-closed): a reviewer that produced
        /// no explicit verdict has not cleared the diff.
        Unknown,
    }

    impl ReviewVerdict {
        /// True unless the reviewer explicitly signed off. Fail-closed: a
        /// `Block` OR an `Unknown` verdict both gate the merge.
        #[must_use]
        pub fn is_blocking(self) -> bool {
            !matches!(self, ReviewVerdict::Pass)
        }

        /// Lowercase wire label used in the aggregated result JSON.
        #[must_use]
        pub fn label(self) -> &'static str {
            match self {
                ReviewVerdict::Pass => "pass",
                ReviewVerdict::Block => "block",
                ReviewVerdict::Unknown => "unknown",
            }
        }
    }

    /// M22 C5: parse a review worker's report into a [`ReviewVerdict`].
    /// Scans for the LAST line containing [`REVIEW_VERDICT_MARKER`]
    /// (case-insensitive) and reads the token after it: `pass`/`ok`/
    /// `approve` → [`ReviewVerdict::Pass`]; `block`/`fail`/`reject` →
    /// [`ReviewVerdict::Block`]; anything else — or no marker at all —
    /// [`ReviewVerdict::Unknown`]. Taking the LAST marker means a reviewer
    /// that quotes the protocol earlier in prose and states its real verdict
    /// at the end is read correctly. Shared with the runner-side join.
    #[must_use]
    pub fn parse_review_verdict(report: &str) -> ReviewVerdict {
        let mut verdict = ReviewVerdict::Unknown;
        for line in report.lines() {
            // The marker is ASCII and `to_ascii_uppercase` is byte-length
            // preserving, so the found index is a valid boundary into the
            // ORIGINAL line too.
            let Some(idx) = line.to_ascii_uppercase().find(REVIEW_VERDICT_MARKER) else {
                continue;
            };
            let tail = line[idx + REVIEW_VERDICT_MARKER.len()..]
                .trim()
                .to_ascii_lowercase();
            let token = tail.split_whitespace().next().unwrap_or("");
            verdict = match token {
                "pass" | "ok" | "approve" | "approved" => ReviewVerdict::Pass,
                "block" | "fail" | "reject" | "rejected" => ReviewVerdict::Block,
                _ => ReviewVerdict::Unknown,
            };
        }
        verdict
    }

    /// M22 C5: build a review-role worker's instructions. The worker is told
    /// NOT to write feature code, to drive the existing `code-review` skill
    /// (the same skill `self_review.rs` points at) over the supplied `diff`
    /// / the sibling worker branches, and to end its report with a
    /// [`REVIEW_VERDICT_MARKER`] line so the merge can be gated on it. The
    /// leading [`REVIEWER_ROLE_SENTINEL`] lets the runner-side join
    /// recognize the reviewer.
    fn review_instructions(task: &str, diff: Option<&str>) -> String {
        let diff_block = match diff {
            Some(d) => format!("\n\n--- DIFF TO REVIEW ---\n{d}\n--- END DIFF ---"),
            None => String::new(),
        };
        format!(
            "{REVIEWER_ROLE_SENTINEL} You are the REVIEWER for this batch — do NOT write feature \
             code. Review the changes (the diff below, and — if you were given a /workspace \
             worktree — the sibling worker branches via `git diff main..sib/<id>`) and decide \
             whether they are safe to merge.\n\n\
             Steps:\n\
             1. `load_skill(\"code-review\")` and follow its checklist: correctness first, then \
             the adversarial pass, edge cases, and security.\n\
             2. Read the changes carefully.\n\
             3. List each BLOCKING issue on its own line (file:line + what's wrong).\n\n\
             You MUST end your report with EXACTLY ONE verdict line:\n\
             `{REVIEW_VERDICT_MARKER} pass`  — no blocking issues, safe to merge; OR\n\
             `{REVIEW_VERDICT_MARKER} block` — blocking issues found, do NOT merge.\n\n\
             Reviewer task: {task}{diff_block}"
        )
    }

    /// M22 C5: one reviewer's outcome in the aggregated result JSON.
    #[derive(Debug, serde::Serialize)]
    struct ReviewOutcomeJson {
        name: String,
        verdict: &'static str,
        blocking: bool,
        /// The reviewer's report (its findings) — only when it blocks.
        #[serde(skip_serializing_if = "Option::is_none")]
        findings: Option<String>,
    }

    /// M22 C5: the merge gate computed from the reviewers' outcomes.
    struct ReviewGate {
        blocked: bool,
        reviewers: Vec<ReviewOutcomeJson>,
    }

    /// M22 C5: evaluate the merge gate over the joined outcome. A reviewer
    /// that did not report (spawn-failed / timed out) cannot clear the diff
    /// and so blocks (fail-closed); a reviewer that reported is classified
    /// by [`parse_review_verdict`]. The merge is blocked if ANY reviewer is
    /// blocking.
    fn evaluate_review_gate(
        reviewer_names: &[String],
        outcome: &crate::context::DelegateBatchOutcome,
    ) -> ReviewGate {
        let mut reviewers = Vec::with_capacity(reviewer_names.len());
        let mut blocked = false;
        for name in reviewer_names {
            let found = outcome.workers.iter().find(|w| &w.name == name);
            let review = if let Some(report) = found.and_then(|w| w.report.as_deref()) {
                let verdict = parse_review_verdict(report);
                let is_blocking = verdict.is_blocking();
                blocked |= is_blocking;
                ReviewOutcomeJson {
                    name: name.clone(),
                    verdict: verdict.label(),
                    blocking: is_blocking,
                    findings: is_blocking.then(|| report.to_owned()),
                }
            } else {
                // No report: the reviewer spawn-failed or timed out.
                blocked = true;
                ReviewOutcomeJson {
                    name: name.clone(),
                    verdict: "no_report",
                    blocking: true,
                    findings: found.and_then(|w| w.error.clone()),
                }
            };
            reviewers.push(review);
        }
        ReviewGate { blocked, reviewers }
    }

    /// Wrap `contract` (when present) around `instructions`: the contract
    /// text lands FIRST, verbatim, so a caller can assert
    /// `instructions.starts_with(&contract)`; a directive to persist it to
    /// `.copperclaw/CONTRACT.md` follows before the worker's actual task.
    /// `None` returns `instructions` unchanged — the pre-Q7 shape.
    fn worker_instructions(contract: Option<&str>, instructions: String) -> String {
        match contract {
            None => instructions,
            Some(contract) => format!(
                "{contract}\n\n---\nThe text above is the SHARED CONTRACT for this batch: \
                 interfaces, file ownership, and naming conventions every worker must follow. \
                 Before your first edit, write it verbatim to `.copperclaw/CONTRACT.md` in your \
                 /workspace root (so it survives your own compaction), then re-read that file if \
                 you ever lose track of it. Now, your task:\n\n{instructions}"
            ),
        }
    }

    pub fn schema() -> Tool {
        make_tool(
            "delegate_batch",
            "Spawn SEVERAL write-capable build workers IN PARALLEL and BLOCK until they ALL \
             finish, then return their reports as ONE aggregated result. This is the fan-out \
             + JOIN primitive: use it when you want to build/investigate multiple INDEPENDENT \
             pieces at once and then ASSEMBLE the results in the same turn (e.g. 'build the API, \
             the CLI, and the docs in parallel, then wire them together'). Each worker is exactly \
             a `delegate`: its OWN container, its OWN writable git worktree at /workspace on its \
             OWN branch (sib/<id>) if you're in a git repo, CONTAINED (it reports only back to \
             you, never to the user's chat). Point EACH worker at /workspace in its \
             `instructions` ('implement X under /workspace and commit'). After the batch returns \
             you review + merge each worker's branch (`git diff main..sib/<id>`, `git merge \
             sib/<id>`). Fan-out is capped at 6 workers per call — split larger spreads into \
             multiple batches, or use plain `delegate` (called N times) for looser, unbounded \
             fan-out whose results arrive asynchronously instead of joined. The call blocks up \
             to `timeout_secs` (default 300, max 600); a worker that fails to spawn or does not \
             report in time surfaces as a per-worker error in the aggregate — it never loses the \
             whole batch. GOLDEN PATTERN for fan-out builds: (1) WRITE A CONTRACT — before \
             fanning out, author a short shared brief (interfaces, file-ownership map, naming \
             conventions) and pass it as `contract`; it's prepended verbatim to every worker's \
             instructions and each worker persists it to `.copperclaw/CONTRACT.md` so it survives \
             their own compaction. (2) ONE COMPONENT PER WORKER — split by file/module ownership \
             so workers never touch the same files. (3) VERIFY THE UNION — pass `project` (the \
             directory you're building in); if it has a `.copperclaw/verify`, the join marks it \
             dirty so you cannot complete the integration todo until every stage re-passes on the \
             merged tree, not just each worker's isolated branch. REVIEWER ROLE: mark a worker \
             `role: \"review\"` (optionally handing it the `diff` to review) to make it a \
             reviewer instead of a builder — it drives the code-review skill over the changes \
             and ends its report with a verdict. Its findings GATE THE MERGE: if any reviewer \
             blocks, the aggregate carries `merge_blocked: true` plus the findings and you must \
             address them before merging any worker branch. For a QUICK read-only lookup \
             that shares your live workspace, prefer `explore`.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["workers"],
                "properties": {
                    "workers": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_DELEGATE_BATCH_WIDTH,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["name", "instructions"],
                            "properties": {
                                "name": { "type": "string", "minLength": 1 },
                                "instructions": { "type": "string", "minLength": 1 },
                                "role": {
                                    "type": "string",
                                    "enum": ["build", "review"],
                                    "description": "Worker role. Omit (or \"build\") for a \
                                        normal build worker. \"review\" makes it a REVIEWER: it \
                                        drives the code-review skill over the changes and its \
                                        verdict gates the merge — if any reviewer blocks, the \
                                        aggregate is flagged `merge_blocked`."
                                },
                                "diff": {
                                    "type": ["string", "null"],
                                    "minLength": 1,
                                    "description": "For a review worker: the diff to review, \
                                        embedded verbatim into its instructions. Ignored for a \
                                        build worker."
                                }
                            }
                        }
                    },
                    "timeout_secs": {
                        "type": ["integer", "null"],
                        "minimum": 1,
                        "maximum": MAX_DELEGATE_BATCH_TIMEOUT_SECS
                    },
                    "contract": {
                        "type": ["string", "null"],
                        "minLength": 1,
                        "description": "Shared brief (interfaces, file ownership, naming \
                            conventions) prepended verbatim to every worker's instructions and \
                            persisted by each worker as .copperclaw/CONTRACT.md."
                    },
                    "project": {
                        "type": ["string", "null"],
                        "minLength": 1,
                        "description": "Your own project directory (the one you cd'd into before \
                            delegating). When it has a .copperclaw/verify, the join marks it \
                            dirty so the merged union must re-pass every stage before delivery."
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
        if input.workers.is_empty() {
            return Err(ToolError::Validation(
                "`workers` must contain at least one worker".into(),
            ));
        }
        if input.workers.len() > MAX_DELEGATE_BATCH_WIDTH {
            return Err(ToolError::Validation(format!(
                "`delegate_batch` fans out at most {MAX_DELEGATE_BATCH_WIDTH} workers (got {}); \
                 split into multiple batches, or use `delegate` for looser async fan-out",
                input.workers.len()
            )));
        }
        if let Some(contract) = input.contract.as_ref() {
            if contract.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`contract`, when present, must be non-empty".into(),
                ));
            }
        }
        if let Some(project) = input.project.as_ref() {
            if project.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`project`, when present, must be non-empty".into(),
                ));
            }
        }
        // M20 M1: contract-presence counter — one increment per call, not per
        // worker, so the ratio reads as "batches with a contract" not
        // "workers under a contract".
        copperclaw_metrics::inc_delegate_batch_contract(input.contract.is_some());
        let mut workers = Vec::with_capacity(input.workers.len());
        // M22 C5: the names of the review-role workers, in request order, so
        // the post-join gate can match each reviewer's outcome back by name.
        let mut reviewer_names: Vec<String> = Vec::new();
        for w in input.workers {
            if w.name.trim().is_empty() {
                return Err(ToolError::Validation(
                    "each worker `name` must be non-empty".into(),
                ));
            }
            if w.instructions.trim().is_empty() {
                return Err(ToolError::Validation(
                    "each worker `instructions` must be non-empty".into(),
                ));
            }
            let instructions = match w.role.unwrap_or_default() {
                WorkerRole::Build => worker_instructions(input.contract.as_deref(), w.instructions),
                WorkerRole::Review => {
                    // M22 C5: a reviewer drives the code-review skill over the
                    // diff and emits a verdict; the contract (when present) is
                    // folded in as CONTEXT to hold the change against — NOT the
                    // build-worker "persist to CONTRACT.md" directive, since a
                    // reviewer doesn't edit.
                    reviewer_names.push(w.name.clone());
                    let body = review_instructions(&w.instructions, w.diff.as_deref());
                    match input.contract.as_deref() {
                        None => body,
                        Some(contract) => format!(
                            "{contract}\n\n---\nThe text above is the SHARED CONTRACT for this \
                             batch (interfaces, file ownership, naming conventions) — hold the \
                             changes to it.\n\n{body}"
                        ),
                    }
                }
            };
            workers.push(DelegateBatchWorker {
                name: w.name,
                instructions,
            });
        }
        let timeout_secs = input
            .timeout_secs
            .unwrap_or(DEFAULT_DELEGATE_BATCH_TIMEOUT_SECS)
            .clamp(1, MAX_DELEGATE_BATCH_TIMEOUT_SECS);

        let req = DelegateBatchRequest {
            workers,
            timeout_secs,
        };
        let outcome = ctx.run_delegate_batch(req).await?;

        // A batch where EVERY worker failed to spawn (e.g. refused by the
        // subagent depth cap) is a refusal, not a partial result — surface
        // it as a tool error so the model sees a clear "refused", not an
        // aggregate of failures it might try to consume.
        if outcome.all_spawn_failed() {
            // M19 A1: whole-batch refusal (every worker failed to spawn).
            copperclaw_metrics::inc_delegate_batch_refused();
            // M20 M1: the post-join dirty-mark code below is structurally
            // unreachable on this path (nothing merged, nothing to
            // re-verify) — record that under the same outcome metric so a
            // dashboard built on it doesn't need a separate refusal join.
            copperclaw_metrics::inc_delegate_batch_post_join_dirty("skipped_all_spawn_failed");
            let reason = outcome
                .workers
                .first()
                .and_then(|w| w.error.clone())
                .unwrap_or_else(|| "all workers refused".into());
            return Err(ToolError::Context(format!(
                "delegate_batch refused: {reason}"
            )));
        }

        // M20 Q7 (b): at least one worker did real work (we didn't hit the
        // all-spawn-failed refusal above) and the parent named its own
        // project — if that project has a recorded verify, mark it dirty
        // via the EXISTING Q2 gate so the parent cannot complete its
        // integration todo without re-running every stage against the
        // merged union. `verify_gate=off` sessions skip this too (one
        // escape hatch, not two, mirroring Q6).
        if ctx.verify_gate_enabled() {
            if let Some(project) = input.project.as_deref() {
                if let Some(project_root) = verify_gate::project_root_of(project) {
                    let has_verify = verify_gate::recorded_verify_command(
                        &project_root,
                        ctx.check_command_override().as_deref(),
                    )
                    .await
                    .is_some();
                    if has_verify {
                        verify_gate::mark_dirty(&project_root).await;
                        copperclaw_metrics::inc_delegate_batch_post_join_dirty("marked");
                    } else {
                        copperclaw_metrics::inc_delegate_batch_post_join_dirty("skipped_no_verify");
                    }
                } else {
                    copperclaw_metrics::inc_delegate_batch_post_join_dirty("skipped_no_project");
                }
            } else {
                copperclaw_metrics::inc_delegate_batch_post_join_dirty("skipped_no_project");
            }
        } else {
            copperclaw_metrics::inc_delegate_batch_post_join_dirty("skipped_gate_off");
        }

        // M22 C5: no reviewer in the batch → the pre-C5 aggregate shape,
        // byte-for-byte (`{"workers": [...]}`), full back-compat.
        if reviewer_names.is_empty() {
            return Ok(success_json(&outcome));
        }

        // A reviewer was in the batch: classify each reviewer's verdict and
        // gate the merge. This is modeled consistently with how the batch
        // already reports worker results — the findings are surfaced IN the
        // aggregate (not raised as a tool error, which would bury the
        // successful build reports the parent still needs to see) — with a
        // top-level `merge_blocked` flag the parent turn keys off.
        let gate = evaluate_review_gate(&reviewer_names, &outcome);
        // M22 C5 metric wishes (recorded for the M1 rider): reviewers
        // dispatched (`reviewer_names.len()`) and whether the batch was
        // merge-blocked (`gate.blocked`).
        let mut body = serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}));
        if let serde_json::Value::Object(map) = &mut body {
            map.insert("merge_blocked".into(), serde_json::json!(gate.blocked));
            map.insert(
                "review".into(),
                serde_json::json!({
                    "blocked": gate.blocked,
                    "reviewers": gate.reviewers,
                    "hint": if gate.blocked {
                        "A reviewer BLOCKED this batch — do NOT merge the worker branches. \
                         Address the findings above (re-run the affected workers as needed) and \
                         re-review before merging."
                    } else {
                        "All reviewers passed — the worker branches are cleared to merge."
                    },
                }),
            );
        }
        Ok(success_json(&body))
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
    use crate::context::{DelegateSpec, MockToolContext, OutboundToolEffect};
    use crate::error::ToolError;
    use rmcp::model::JsonObject;
    use serde_json::Value;

    fn args_from(value: Value) -> Option<JsonObject> {
        match value {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    #[tokio::test]
    async fn create_agent_happy() {
        let ctx = MockToolContext::new();
        super::create_agent::handle(
            args_from(
                serde_json::json!({"name": "Greeter", "instructions": "Say hi.", "channel": "telegram:chat-1"}),
            ),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::CreateAgent(s) => {
                assert_eq!(s.name, "Greeter");
                assert_eq!(s.channel.as_deref(), Some("telegram:chat-1"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_agent_empty_name() {
        let ctx = MockToolContext::new();
        let err = super::create_agent::handle(
            args_from(serde_json::json!({"name": " ", "instructions": "i"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn create_agent_empty_instructions() {
        let ctx = MockToolContext::new();
        let err = super::create_agent::handle(
            args_from(serde_json::json!({"name": "n", "instructions": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn create_agent_empty_channel() {
        let ctx = MockToolContext::new();
        let err = super::create_agent::handle(
            args_from(serde_json::json!({"name": "n", "instructions": "i", "channel": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn schema_required_fields() {
        let s = super::create_agent::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["name", "instructions"]));
    }

    #[tokio::test]
    async fn delegate_happy_emits_effect() {
        let ctx = MockToolContext::new();
        super::delegate::handle(
            args_from(
                serde_json::json!({"name": "builder", "instructions": "build X under /workspace"}),
            ),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::Delegate(DelegateSpec { name, instructions }) => {
                assert_eq!(name, "builder");
                assert_eq!(instructions, "build X under /workspace");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_empty_name_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate::handle(
            args_from(serde_json::json!({"name": " ", "instructions": "i"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn delegate_empty_instructions_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate::handle(
            args_from(serde_json::json!({"name": "n", "instructions": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn delegate_schema_has_no_channel_field() {
        // A delegate is never user-facing, so it takes no `channel`.
        let s = super::delegate::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["name", "instructions"]));
        assert!(
            v["properties"].get("channel").is_none(),
            "delegate must not expose a channel binding"
        );
    }

    // ── delegate_batch (A1) ──────────────────────────────────────────────

    use crate::context::{
        DelegateBatchOutcome, MAX_DELEGATE_BATCH_TIMEOUT_SECS, MAX_DELEGATE_BATCH_WIDTH,
        WorkerOutcome, WorkerStatus,
    };

    #[tokio::test]
    async fn delegate_batch_happy_aggregates_reports() {
        let ctx = MockToolContext::new();
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "api", "instructions": "build the API under /workspace"},
                    {"name": "cli", "instructions": "build the CLI under /workspace"}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        // The mock recorded the request with both workers and the default budget.
        let calls = ctx.delegate_batch_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workers.len(), 2);
        assert_eq!(
            calls[0].timeout_secs,
            crate::context::DEFAULT_DELEGATE_BATCH_TIMEOUT_SECS
        );
        // The aggregate carries both worker names + their (mock) reports.
        let body = match &out.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            other => panic!("unexpected content block: {other:?}"),
        };
        assert!(body.contains("api"), "got: {body}");
        assert!(body.contains("cli"), "got: {body}");
        assert!(body.contains("mock report from api"), "got: {body}");
    }

    #[tokio::test]
    async fn delegate_batch_empty_workers_rejected() {
        let ctx = MockToolContext::new();
        let err =
            super::delegate_batch::handle(args_from(serde_json::json!({"workers": []})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.delegate_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn delegate_batch_over_width_rejected() {
        let ctx = MockToolContext::new();
        let workers: Vec<_> = (0..=MAX_DELEGATE_BATCH_WIDTH)
            .map(|i| serde_json::json!({"name": format!("w{i}"), "instructions": "x"}))
            .collect();
        let err =
            super::delegate_batch::handle(args_from(serde_json::json!({"workers": workers})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(err, ToolError::Validation(s) if s.contains("at most")));
        assert!(ctx.delegate_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn delegate_batch_empty_worker_field_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": " ", "instructions": "x"}]
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn delegate_batch_timeout_is_clamped() {
        let ctx = MockToolContext::new();
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}],
                "timeout_secs": 999_999
            })),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            ctx.delegate_batch_calls()[0].timeout_secs,
            MAX_DELEGATE_BATCH_TIMEOUT_SECS
        );
    }

    #[tokio::test]
    async fn delegate_batch_all_spawn_failed_is_a_refusal() {
        // When every worker failed to spawn (e.g. the subagent depth cap
        // refused the batch), the tool surfaces a clear tool error, not a
        // partial aggregate.
        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![WorkerOutcome {
                name: "a".into(),
                status: WorkerStatus::SpawnFailed,
                report: None,
                error: Some("nested create_agent (max depth = 3)".into()),
                session_id: None,
            }],
        });
        let err = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}]
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Context(s) if s.contains("refused") && s.contains("max depth")),
            "a fully-refused batch must surface as a refusal tool error"
        );
    }

    #[tokio::test]
    async fn delegate_batch_partial_failure_still_returns_aggregate() {
        // A worker failure surfaces as a per-worker error in the aggregate,
        // NOT a lost turn — as long as at least one worker didn't spawn-fail.
        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![
                WorkerOutcome {
                    name: "ok".into(),
                    status: WorkerStatus::Ok,
                    report: Some("done".into()),
                    error: None,
                    session_id: Some("11111111-1111-1111-1111-111111111111".into()),
                },
                WorkerOutcome {
                    name: "slow".into(),
                    status: WorkerStatus::Timeout,
                    report: None,
                    error: Some("worker did not report within 300s".into()),
                    session_id: Some("22222222-2222-2222-2222-222222222222".into()),
                },
            ],
        });
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "ok", "instructions": "x"},
                    {"name": "slow", "instructions": "y"}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let body = match &out.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            other => panic!("unexpected content block: {other:?}"),
        };
        assert!(body.contains("done"), "got: {body}");
        assert!(body.contains("did not report"), "got: {body}");
    }

    #[test]
    fn delegate_batch_schema_shape() {
        let s = super::delegate_batch::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["workers"]));
        assert_eq!(
            v["properties"]["workers"]["maxItems"],
            serde_json::json!(MAX_DELEGATE_BATCH_WIDTH)
        );
        // A worker takes exactly name + instructions (no channel).
        assert!(
            v["properties"]["workers"]["items"]["properties"]
                .get("channel")
                .is_none()
        );
        // M20 Q7: `contract` and `project` are both optional (not required).
        assert!(v["properties"].get("contract").is_some());
        assert!(v["properties"].get("project").is_some());
    }

    // ── delegate_batch contract + post-join verify (M20 Q7) ──────────────

    /// RAII guard pointing `verify_gate`'s data root at a fresh tempdir,
    /// mirroring `computer_use.rs`'s own `DataRootGuard` — shares
    /// `verify_gate`'s test lock so these tests never race its own or
    /// `computer_use.rs`'s / `todo.rs`'s data-root overrides.
    struct DataRootGuard {
        dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl DataRootGuard {
        fn new() -> Self {
            let lock = crate::tools::verify_gate::data_root_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            crate::tools::verify_gate::data_root_test_override_set(dir.path().to_path_buf());
            Self { dir, _lock: lock }
        }

        fn path(&self) -> &std::path::Path {
            self.dir.path()
        }
    }

    impl Drop for DataRootGuard {
        fn drop(&mut self) {
            crate::tools::verify_gate::data_root_test_override_clear();
        }
    }

    #[tokio::test]
    async fn delegate_batch_contract_prepended_to_every_worker() {
        // A 3-worker batch each receives the contract text: prepended
        // VERBATIM to its instructions, plus a directive to persist it as
        // `.copperclaw/CONTRACT.md` in its own worktree.
        let ctx = MockToolContext::new();
        let contract = "## Shared contract\n- API returns JSON\n- CLI owns src/cli/**";
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "api", "instructions": "build the API under /workspace"},
                    {"name": "cli", "instructions": "build the CLI under /workspace"},
                    {"name": "docs", "instructions": "write docs under /workspace"}
                ],
                "contract": contract
            })),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.delegate_batch_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workers.len(), 3);
        for w in &calls[0].workers {
            assert!(
                w.instructions.starts_with(contract),
                "worker {} instructions must start with the contract verbatim, got: {}",
                w.name,
                w.instructions
            );
            assert!(
                w.instructions.contains(".copperclaw/CONTRACT.md"),
                "worker {} instructions must direct it to persist the contract, got: {}",
                w.name,
                w.instructions
            );
        }
        // The worker's own task text still follows the contract.
        assert!(calls[0].workers[0].instructions.contains("build the API"));
        assert!(calls[0].workers[1].instructions.contains("build the CLI"));
        assert!(calls[0].workers[2].instructions.contains("write docs"));
    }

    #[tokio::test]
    async fn delegate_batch_no_contract_is_byte_identical_to_pre_q7() {
        // Back-compat: a batch WITHOUT a contract sends instructions
        // completely unchanged — no prefix, no directive text at all.
        let ctx = MockToolContext::new();
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "api", "instructions": "build the API under /workspace"},
                    {"name": "cli", "instructions": "build the CLI under /workspace"}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.delegate_batch_calls();
        assert_eq!(
            calls[0].workers[0].instructions,
            "build the API under /workspace"
        );
        assert_eq!(
            calls[0].workers[1].instructions,
            "build the CLI under /workspace"
        );
    }

    #[tokio::test]
    async fn delegate_batch_empty_contract_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}],
                "contract": "   "
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.delegate_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn delegate_batch_empty_project_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}],
                "project": ""
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.delegate_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn delegate_batch_post_join_marks_parent_project_dirty() {
        // Post-join, a parent project with a recorded `.copperclaw/verify`
        // is marked dirty so the integration todo can't complete until every
        // stage re-passes on the merged union.
        let g = DataRootGuard::new();
        let proj = g.path().join("habit-tracker");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(
            proj.join(".copperclaw").join("verify"),
            "lint: npx eslint .\ntypecheck: tsc --noEmit\n",
        )
        .unwrap();
        assert!(!crate::tools::verify_gate::is_dirty(&proj).await);

        let ctx = MockToolContext::new();
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "api", "instructions": "build the API under /workspace"},
                    {"name": "cli", "instructions": "build the CLI under /workspace"},
                    {"name": "docs", "instructions": "write docs under /workspace"}
                ],
                "contract": "shared brief",
                "project": proj.to_string_lossy()
            })),
            &ctx,
        )
        .await
        .unwrap();

        assert!(
            crate::tools::verify_gate::is_dirty(&proj).await,
            "the parent project must be dirty after the join so verify stages re-run \
             against the merged tree"
        );
    }

    #[tokio::test]
    async fn delegate_batch_no_project_leaves_verify_gate_untouched() {
        // Back-compat: a batch without `project` never touches the
        // verify-gate state at all — pre-Q7 callers see zero new behavior.
        let g = DataRootGuard::new();
        let proj = g.path().join("untouched");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "test: true\n").unwrap();

        let ctx = MockToolContext::new();
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "api", "instructions": "build the API under /workspace"}]
            })),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!crate::tools::verify_gate::is_dirty(&proj).await);
    }

    #[tokio::test]
    async fn delegate_batch_project_without_verify_file_is_a_noop() {
        // A named project with no recorded `.copperclaw/verify` is not
        // marked dirty — there is nothing to re-verify.
        let g = DataRootGuard::new();
        let proj = g.path().join("no-verify-yet");
        std::fs::create_dir_all(&proj).unwrap();

        let ctx = MockToolContext::new();
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "api", "instructions": "x"}],
                "project": proj.to_string_lossy()
            })),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!crate::tools::verify_gate::is_dirty(&proj).await);
    }

    #[tokio::test]
    async fn delegate_batch_all_spawn_failed_does_not_mark_project_dirty() {
        // A fully-refused batch (nothing actually ran) must not dirty the
        // parent project — there's no merged union to re-verify.
        let g = DataRootGuard::new();
        let proj = g.path().join("refused");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "test: true\n").unwrap();

        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![WorkerOutcome {
                name: "a".into(),
                status: WorkerStatus::SpawnFailed,
                report: None,
                error: Some("nested create_agent (max depth = 3)".into()),
                session_id: None,
            }],
        });
        let _ = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}],
                "project": proj.to_string_lossy()
            })),
            &ctx,
        )
        .await
        .unwrap_err();

        assert!(!crate::tools::verify_gate::is_dirty(&proj).await);
    }

    #[tokio::test]
    async fn delegate_batch_verify_gate_off_skips_dirty_mark() {
        // The `verify_gate=off` escape hatch (mirroring Q6) applies here
        // too — one escape hatch, not two.
        let g = DataRootGuard::new();
        let proj = g.path().join("gate-off");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "test: true\n").unwrap();

        let ctx = MockToolContext::new();
        ctx.set_verify_gate_enabled(false);
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}],
                "project": proj.to_string_lossy()
            })),
            &ctx,
        )
        .await
        .unwrap();

        assert!(!crate::tools::verify_gate::is_dirty(&proj).await);
    }

    // ── delegate_batch reviewer role + merge gate (M22 C5) ───────────────

    use super::delegate_batch::{ReviewVerdict, parse_review_verdict};

    /// Extract the JSON body of a `delegate_batch` success result.
    fn result_body(out: &rmcp::model::CallToolResult) -> Value {
        let text = match &out.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            other => panic!("unexpected content block: {other:?}"),
        };
        serde_json::from_str(&text).expect("aggregate is JSON")
    }

    #[test]
    fn parse_review_verdict_reads_pass_block_and_unknown() {
        assert_eq!(
            parse_review_verdict("looks good\nREVIEW-VERDICT: pass"),
            ReviewVerdict::Pass
        );
        assert_eq!(
            parse_review_verdict("api.py:4 no validation\nreview-verdict: BLOCK"),
            ReviewVerdict::Block
        );
        // No marker at all → Unknown (fail-closed).
        assert_eq!(
            parse_review_verdict("I skimmed it, LGTM"),
            ReviewVerdict::Unknown
        );
        // The LAST marker wins — a quoted protocol earlier doesn't fool it.
        assert_eq!(
            parse_review_verdict(
                "The protocol is `REVIEW-VERDICT: pass` or block.\n\
                 Found a null deref.\nREVIEW-VERDICT: block"
            ),
            ReviewVerdict::Block
        );
        assert!(ReviewVerdict::Block.is_blocking());
        assert!(ReviewVerdict::Unknown.is_blocking());
        assert!(!ReviewVerdict::Pass.is_blocking());
    }

    #[tokio::test]
    async fn delegate_batch_review_role_dispatched_with_diff() {
        // Acceptance (unit): a `role: "review"` worker is dispatched with the
        // diff payload embedded and directed to drive the code-review skill +
        // emit a verdict.
        let ctx = MockToolContext::new();
        let diff = "--- a/api.py\n+++ b/api.py\n+def handler(): pass\n";
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "impl", "instructions": "build the API under /workspace"},
                    {"name": "reviewer", "instructions": "review the API change",
                     "role": "review", "diff": diff}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.delegate_batch_calls();
        assert_eq!(calls.len(), 1);
        let rev = calls[0]
            .workers
            .iter()
            .find(|w| w.name == "reviewer")
            .expect("reviewer dispatched");
        // The reviewer received the diff verbatim …
        assert!(rev.instructions.contains(diff), "got: {}", rev.instructions);
        // … is told to drive the code-review skill …
        assert!(rev.instructions.contains("load_skill(\"code-review\")"));
        // … must emit the machine-readable verdict …
        assert!(rev.instructions.contains("REVIEW-VERDICT:"));
        // … and carries the sentinel the runner-side join keys off.
        assert!(
            rev.instructions
                .contains(super::delegate_batch::REVIEWER_ROLE_SENTINEL)
        );
        // A build worker is unaffected — no reviewer scaffolding leaks in.
        let impl_w = calls[0].workers.iter().find(|w| w.name == "impl").unwrap();
        assert!(!impl_w.instructions.contains("REVIEW-VERDICT:"));
    }

    #[tokio::test]
    async fn delegate_batch_review_block_gates_merge() {
        // Acceptance (integration): a batch whose reviewer reports blocking
        // findings flags `merge_blocked` and surfaces the findings, while the
        // build worker's report is still delivered.
        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![
                WorkerOutcome {
                    name: "impl".into(),
                    status: WorkerStatus::Ok,
                    report: Some("built the endpoint".into()),
                    error: None,
                    session_id: Some("11111111-1111-1111-1111-111111111111".into()),
                },
                WorkerOutcome {
                    name: "reviewer".into(),
                    status: WorkerStatus::Ok,
                    report: Some(
                        "api.py:4 handler swallows errors\nno auth check\nREVIEW-VERDICT: block"
                            .into(),
                    ),
                    error: None,
                    session_id: Some("22222222-2222-2222-2222-222222222222".into()),
                },
            ],
        });
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "impl", "instructions": "build it"},
                    {"name": "reviewer", "instructions": "review it", "role": "review"}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let body = result_body(&out);
        assert_eq!(body["merge_blocked"], serde_json::json!(true));
        assert_eq!(body["review"]["blocked"], serde_json::json!(true));
        // The reviewer's findings are surfaced …
        let findings = body["review"]["reviewers"][0]["findings"].as_str().unwrap();
        assert!(findings.contains("swallows errors"), "got: {findings}");
        // … and the build worker's report is still present in the aggregate.
        assert!(
            body["workers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["report"].as_str() == Some("built the endpoint"))
        );
    }

    #[tokio::test]
    async fn delegate_batch_review_pass_clears_merge() {
        // A reviewer that signs off does NOT block: `merge_blocked` is false
        // and no findings are attached.
        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![WorkerOutcome {
                name: "reviewer".into(),
                status: WorkerStatus::Ok,
                report: Some("read the diff, clean\nREVIEW-VERDICT: pass".into()),
                error: None,
                session_id: Some("22222222-2222-2222-2222-222222222222".into()),
            }],
        });
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "reviewer", "instructions": "review it", "role": "review"}]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let body = result_body(&out);
        assert_eq!(body["merge_blocked"], serde_json::json!(false));
        assert_eq!(body["review"]["reviewers"][0]["verdict"], "pass");
        assert!(body["review"]["reviewers"][0].get("findings").is_none());
    }

    #[tokio::test]
    async fn delegate_batch_reviewer_that_did_not_report_blocks() {
        // Fail-closed: a reviewer that timed out (no report) cannot clear the
        // diff, so the merge is blocked.
        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![WorkerOutcome {
                name: "reviewer".into(),
                status: WorkerStatus::Timeout,
                report: None,
                error: Some("worker did not report within 300s".into()),
                session_id: Some("22222222-2222-2222-2222-222222222222".into()),
            }],
        });
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "reviewer", "instructions": "review it", "role": "review"}]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let body = result_body(&out);
        assert_eq!(body["merge_blocked"], serde_json::json!(true));
        assert_eq!(body["review"]["reviewers"][0]["verdict"], "no_report");
    }

    #[tokio::test]
    async fn delegate_batch_without_reviewer_omits_the_gate() {
        // Back-compat: a build-only batch returns the pre-C5 aggregate shape
        // with NO `merge_blocked` / `review` keys.
        let ctx = MockToolContext::new();
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "api", "instructions": "build the API under /workspace"},
                    {"name": "cli", "instructions": "build the CLI under /workspace"}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let body = result_body(&out);
        assert!(body.get("merge_blocked").is_none());
        assert!(body.get("review").is_none());
        assert!(body.get("workers").is_some());
    }
}
