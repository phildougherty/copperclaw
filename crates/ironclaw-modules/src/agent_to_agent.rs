//! Resolves outbound destinations of the form `agent:<name>` and handles the
//! `create_agent` system-action.
//!
//! Two responsibilities live in this module:
//!
//! 1. **Destination parsing** — when an agent calls
//!    `send_message(to: "agent:helper")` the runner serializes the destination
//!    string verbatim. The host's delivery loop calls into this module's
//!    [`parse`] / [`is_agent_destination`] helpers to decide whether to route
//!    through a channel adapter or fan the message into another agent's
//!    `messages_in`.
//!
//! 2. **`create_agent` delivery action** — when an agent calls the
//!    `create_agent` MCP tool, the runner writes a `kind=system` outbound row
//!    with content `{"create_agent": {"name": "...", "instructions": "...",
//!    "channel": "..."}}`. The host's delivery loop parses the action name
//!    and dispatches to [`CreateAgentHandler::handle`], which:
//!
//!    a. Permission-gates via the configured closure (defaults to deny so
//!       production wiring must opt in).
//!    b. Refuses if accepting the request would push the new group past the
//!       configured subagent-depth cap (default [`DEFAULT_MAX_SUBAGENT_DEPTH`]).
//!       Depth = parent's depth + 1 (or 1 when the parent is itself an
//!       un-spawned agent, e.g. the initial agent in the install).
//!    c. INSERTs `agent_groups` + `sessions` (+ optional `messaging_group_agents`).
//!    d. Writes a `create_agent_result` system row into the *parent* session's
//!       `inbound.db` so the calling agent sees the real id on its next turn.
//!
//! The container manager's reconcile loop polls the `sessions` table on a
//! short timer, so the new agent's container will spawn on its next tick
//! without any explicit notification from the handler.

use crate::context::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput, InterceptorCtx,
    InterceptorDecision, Module, ModuleContext,
};
use crate::error::ModuleError;
use async_trait::async_trait;
use chrono::Utc;
use ironclaw_db::central::CentralDb;
use ironclaw_db::session::SessionPaths;
use ironclaw_db::tables::{
    agent_groups::{self, CreateAgentGroup},
    messages_in::{self, WriteInbound},
    messaging_group_agents::{self, UpsertWiring},
    messaging_groups::{self, UpsertMessagingGroup},
    sessions::{self, CreateSession},
    user_roles,
};
use ironclaw_types::{
    AgentGroupId, ChannelType, EngageMode, MessageId, MessageKind, SessionId, SessionMode,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The `agent:` URL prefix.
pub const AGENT_PREFIX: &str = "agent:";

/// Default channel a `create_agent` call binds to when the payload omits one.
pub const DEFAULT_CREATE_AGENT_CHANNEL: &str = "cli";

/// Platform identifier used for synthetic messaging-groups created via
/// `create_agent`. The spawned agent has no real channel platform — it's
/// addressable only by other agents — so we use a stable "agent-spawn"
/// placeholder so the wiring is unique per-agent.
const SPAWN_PLATFORM_PREFIX: &str = "agent-spawn:";

/// Parsed agent destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    /// Bare agent name (folder slug or display name as configured in the
    /// destinations table).
    pub name: String,
}

/// `true` if `s` looks like an agent destination (`agent:<...>` or
/// `agent://<...>`).
pub fn is_agent_destination(s: &str) -> bool {
    parse(s).is_some()
}

/// Parse `agent:<name>` or `agent://<name>` strings.
pub fn parse(s: &str) -> Option<AgentRef> {
    let s = s.trim();
    let after = s
        .strip_prefix("agent://")
        .or_else(|| s.strip_prefix(AGENT_PREFIX))?;
    let name = after.trim();
    if name.is_empty() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    Some(AgentRef {
        name: name.to_owned(),
    })
}

/// Context handed to a [`CreateAgentPermissionCheck`]. Carries enough
/// state for the check to consult `users` / `user_roles` against the
/// parent session's scope. New fields can be added at any time — the
/// struct is not stable API; production code uses the
/// [`users_table_check`] factory.
#[derive(Debug, Clone)]
pub struct CreateAgentPermissionCtx {
    /// Parent session's agent group, when the action handler could
    /// resolve one. `None` for orphan invocations (no parent session
    /// matched) — those represent administrative / scripted calls.
    pub parent_agent_group_id: Option<AgentGroupId>,
    /// Parent session id, when available.
    pub parent_session_id: Option<SessionId>,
    /// `create_agent` payload's requested name. Surfaced for audit
    /// purposes; the default check ignores it.
    pub requested_name: String,
}

/// Permission closure consulted before a `create_agent` action runs.
/// Returning `false` causes the handler to write a `status: "denied"`
/// result row and abort the central-DB mutation.
///
/// In production the host wires this to a check against the `users` /
/// `user_roles` table via [`users_table_check`]. Tests can use
/// [`always_allow`] / [`always_deny`].
pub type CreateAgentPermissionCheck =
    Arc<dyn Fn(&CreateAgentPermissionCtx) -> bool + Send + Sync>;

/// Module wraps the helpers above and registers a message interceptor that
/// tags outbound messages whose destination resolves to an agent. Kept as a
/// unit struct so existing call sites (`Box::new(AgentToAgentModule)`) keep
/// compiling; the `create_agent` delivery action is registered separately
/// by [`CreateAgentModule`].
pub struct AgentToAgentModule;

/// Companion module that registers the `create_agent` delivery action. The
/// handler runs in-process against the central DB, so this module is the
/// natural place to plumb DB handles into the host's hook surface.
///
/// TODO(team-ca): the host's `boot::install_modules` currently constructs
/// `Box::new(AgentToAgentModule)` (a unit struct) and the file is outside
/// this team's scope to modify. Until the wiring is added in boot.rs, hosts
/// that want the `create_agent` action working must construct this companion
/// module themselves and install it alongside `AgentToAgentModule`.
pub struct CreateAgentModule {
    deps: HandlerDeps,
}

/// Default cap on `create_agent` nesting depth. A parent at depth N can
/// spawn a child at depth N+1; the spawn is rejected once N+1 exceeds
/// this cap. 3 lets a top-level agent delegate to a sibling that
/// delegates to a focused sub-sibling — useful for layered
/// investigations — without permitting an unbounded fork-bomb.
pub const DEFAULT_MAX_SUBAGENT_DEPTH: u8 = 3;

/// Hard ceiling on operator-configured subagent depth caps. Deeper
/// chains than this are misconfiguration: they invite the saturation
/// collapse `checked_add` guards against, and they have no real-world
/// use case beyond fork-bombs.
pub const MAX_SUBAGENT_DEPTH_CEILING: u8 = 16;

#[derive(Clone)]
struct HandlerDeps {
    central: CentralDb,
    data_root: PathBuf,
    permission_check: CreateAgentPermissionCheck,
    /// In-memory `(agent_group_id → depth)` cache. Persisted ground
    /// truth lives in `agent_groups.subagent_depth`; the cache is a
    /// write-through accelerator that avoids hitting the DB twice per
    /// `create_agent` and serves as the synchronisation point for the
    /// re-check-on-insert that prevents the depth-cap TOCTOU race.
    spawned: Arc<Mutex<HashMap<AgentGroupId, u8>>>,
    /// Hard cap on subagent depth — see [`DEFAULT_MAX_SUBAGENT_DEPTH`].
    /// `1` reproduces the historical "no nested spawns at all" rule.
    max_depth: u8,
}

impl Default for AgentToAgentModule {
    fn default() -> Self {
        Self
    }
}

impl CreateAgentModule {
    /// Build a module with the `create_agent` delivery action wired up.
    ///
    /// `central` is the host's central DB (where `agent_groups`, `sessions`,
    /// `messaging_group_agents` live). `data_root` is the on-disk root that
    /// `SessionPaths::new` walks to find each session's `inbound.db`.
    ///
    /// `permission_check` is consulted at every `handle()` call. Pass
    /// [`always_allow`] for tests and a `users`-table lookup in production.
    pub fn new(
        central: CentralDb,
        data_root: impl Into<PathBuf>,
        permission_check: CreateAgentPermissionCheck,
    ) -> Self {
        Self {
            deps: HandlerDeps {
                central,
                data_root: data_root.into(),
                permission_check,
                spawned: Arc::new(Mutex::new(HashMap::new())),
                max_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            },
        }
    }

    /// Override the subagent-depth cap. Values < 1 are clamped to 1 so
    /// the gate never accidentally rejects every spawn; values above
    /// [`MAX_SUBAGENT_DEPTH_CEILING`] are clamped down with a warn,
    /// because deeper chains both invite u8 saturation collapse and
    /// have no plausible legitimate use case.
    #[must_use]
    pub fn with_max_depth(mut self, depth: u8) -> Self {
        let clamped = depth.clamp(1, MAX_SUBAGENT_DEPTH_CEILING);
        if clamped != depth {
            warn!(
                requested = depth,
                clamped,
                ceiling = MAX_SUBAGENT_DEPTH_CEILING,
                "with_max_depth: clamped subagent depth cap to ceiling",
            );
        }
        self.deps.max_depth = clamped;
        self
    }

    /// Test-only helper: borrow the deps so a test can assert against the
    /// internal `spawned` set or central DB.
    #[cfg(test)]
    fn deps(&self) -> &HandlerDeps {
        &self.deps
    }
}

/// Convenience permission closure that always allows. Useful for tests and
/// non-multi-user deployments where every agent is trusted.
pub fn always_allow() -> CreateAgentPermissionCheck {
    Arc::new(|_ctx: &CreateAgentPermissionCtx| true)
}

/// Convenience permission closure that always denies.
pub fn always_deny() -> CreateAgentPermissionCheck {
    Arc::new(|_ctx: &CreateAgentPermissionCtx| false)
}

/// Production permission check: allow `create_agent` only when the
/// install has at least one user granted [`user_roles::Role::Owner`] or
/// [`user_roles::Role::Admin`] (either globally or scoped to the parent
/// agent group). This is the bootstrap form of the role check — it
/// requires an operator to deliberately grant a privileged role before
/// any agent can spawn new agents, but does not yet bind the action to
/// a specific user identity (the system action carries no user
/// context; binding to a user requires per-turn provenance which the
/// schema does not currently track).
///
/// Operationally:
/// * Fresh install with no role grants → deny (safe default).
/// * Operator runs `iclaw users grant <id> admin` or `owner` → allow.
/// * Database read errors → deny (fail-closed).
///
/// The check consults the database on every call so role revocations
/// take effect immediately; a stale cache would extend privilege past
/// the operator's intent.
pub fn users_table_check(central: CentralDb) -> CreateAgentPermissionCheck {
    Arc::new(move |ctx: &CreateAgentPermissionCtx| {
        // Global owner/admin: grants the privilege for every parent.
        let has_global = matches!(
            user_roles::list_for_scope(&central, None, user_roles::Role::Owner),
            Ok(v) if !v.is_empty()
        ) || matches!(
            user_roles::list_for_scope(&central, None, user_roles::Role::Admin),
            Ok(v) if !v.is_empty()
        );
        if has_global {
            return true;
        }
        // Group-scoped owner/admin: grants the privilege only when the
        // parent session resolves to that scope. Orphan calls (no
        // parent) fall through to deny since there's nothing to scope
        // against.
        if let Some(parent_ag) = ctx.parent_agent_group_id {
            let group_owner = matches!(
                user_roles::list_for_scope(&central, Some(parent_ag), user_roles::Role::Owner),
                Ok(v) if !v.is_empty()
            );
            let group_admin = matches!(
                user_roles::list_for_scope(&central, Some(parent_ag), user_roles::Role::Admin),
                Ok(v) if !v.is_empty()
            );
            if group_owner || group_admin {
                return true;
            }
        }
        false
    })
}

#[async_trait]
impl Module for AgentToAgentModule {
    fn name(&self) -> &'static str {
        "agent_to_agent"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        ctx.set_message_interceptor(Arc::new(|i: InterceptorCtx| {
            // If the outbound destination's channel_type is `agent`, the host's
            // delivery loop already routes by `agent_group_id`. We pass it
            // through unchanged. The interceptor exists so the host has a hook
            // to log or rewrite agent-bound messages.
            if i
                .channel_type
                .as_ref()
                .is_some_and(|c| c.as_str() == ChannelType::AGENT)
            {
                return InterceptorDecision::Passthrough;
            }
            // For non-agent destinations, also a pass-through — the module's
            // raison d'être is the helper functions, not interception.
            InterceptorDecision::Passthrough
        }));
        Ok(())
    }
}

#[async_trait]
impl Module for CreateAgentModule {
    fn name(&self) -> &'static str {
        "create_agent"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        ctx.register_delivery_action(
            "create_agent",
            Arc::new(CreateAgentHandler {
                deps: self.deps.clone(),
            }),
        );
        Ok(())
    }
}

/// Delivery-action handler for the runner's `create_agent` system message.
pub struct CreateAgentHandler {
    deps: HandlerDeps,
}

/// Outcome of a single `create_agent` invocation. The handler writes a JSON
/// rendering of this back to the parent session's inbound.db as a `system`
/// row so the parent agent can see the real ids on its next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultStatus {
    Created,
    Denied,
    Rejected,
    Invalid,
}

impl ResultStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Denied => "denied",
            Self::Rejected => "rejected",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Clone)]
struct CreateAgentPayload {
    name: String,
    instructions: String,
    channel: Option<String>,
}

impl CreateAgentPayload {
    fn parse(v: &serde_json::Value) -> Result<Self, String> {
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        let instructions = v
            .get("instructions")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        let channel = v
            .get("channel")
            .and_then(|x| x.as_str())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        if name.is_empty() {
            return Err("create_agent: missing `name`".into());
        }
        if instructions.is_empty() {
            return Err("create_agent: missing `instructions`".into());
        }
        Ok(Self {
            name,
            instructions,
            channel,
        })
    }

    /// Folder slug — lower-case alphanumerics, dashes only.
    fn folder(&self) -> String {
        let mut s = String::with_capacity(self.name.len());
        for c in self.name.chars() {
            if c.is_ascii_alphanumeric() {
                s.push(c.to_ascii_lowercase());
            } else if matches!(c, ' ' | '-' | '_' | '.') {
                s.push('-');
            }
        }
        // Disambiguate concurrent creates that collapse to the same slug.
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let suffix = &suffix[..8];
        if s.is_empty() {
            format!("agent-{suffix}")
        } else {
            format!("{s}-{suffix}")
        }
    }
}

impl DeliveryActionHandler for CreateAgentHandler {
    #[allow(clippy::too_many_lines)] // single linear flow; splitting hurts clarity.
    fn handle(
        &self,
        input: DeliveryActionInput,
    ) -> Result<DeliveryActionOutput, ModuleError> {
        // 1. Parse the payload up-front. A malformed payload is a usage error;
        //    we surface it back to the parent (if we can resolve one) with
        //    `status="invalid"`.
        let payload = match CreateAgentPayload::parse(&input.payload) {
            Ok(p) => p,
            Err(reason) => {
                let parent = self.resolve_parent(&input);
                self.write_parent_result(
                    parent.as_ref(),
                    ResultStatus::Invalid,
                    None,
                    None,
                    Some(reason.as_str()),
                );
                return Ok(DeliveryActionOutput::default());
            }
        };

        // 2. Permission gate. Denied requests are surfaced back as a system
        //    row so the parent agent can adjust its behavior.
        let parent_for_check = self.resolve_parent(&input);
        let perm_ctx = CreateAgentPermissionCtx {
            parent_agent_group_id: parent_for_check.as_ref().map(|p| p.agent_group_id),
            parent_session_id: parent_for_check.as_ref().map(|p| p.session_id),
            requested_name: payload.name.clone(),
        };
        if !(self.deps.permission_check)(&perm_ctx) {
            warn!(
                name = %payload.name,
                "create_agent denied: permissions.create_agent not granted",
            );
            self.write_parent_result(
                parent_for_check.as_ref(),
                ResultStatus::Denied,
                None,
                None,
                Some("permissions.create_agent not granted"),
            );
            return Ok(DeliveryActionOutput::default());
        }

        // 3. Nesting gate (soft check). Compute the depth a new child
        //    would land at: parent's recorded depth + 1, or 1 when the
        //    parent isn't a previously-spawned agent. Reject up-front
        //    when the cap is already obviously exceeded so we skip the
        //    DB writes. The authoritative re-check happens under the
        //    `spawned` lock at insert time (step 4) to close the
        //    TOCTOU race between concurrent calls from the same
        //    parent. Parent depth is read from the central DB so the
        //    gate survives host restarts; the in-memory cache is a
        //    write-through accelerator.
        let parent_session = parent_for_check;
        let parent_depth = self.lookup_parent_depth(parent_session.as_ref());
        let Some(new_depth) = parent_depth.unwrap_or(0).checked_add(1) else {
            warn!(
                parent_agent_group = ?parent_session.as_ref().map(|p| p.agent_group_id.as_uuid()),
                parent_depth = parent_depth.unwrap_or(0),
                max_depth = self.deps.max_depth,
                name = %payload.name,
                "create_agent rejected: parent depth at u8::MAX, child would overflow",
            );
            self.write_parent_result(
                parent_session.as_ref(),
                ResultStatus::Rejected,
                None,
                None,
                Some("nested create_agent (parent depth saturated)"),
            );
            return Ok(DeliveryActionOutput::default());
        };
        if new_depth > self.deps.max_depth {
            warn!(
                parent_agent_group = ?parent_session.as_ref().map(|p| p.agent_group_id.as_uuid()),
                parent_depth = parent_depth.unwrap_or(0),
                max_depth = self.deps.max_depth,
                name = %payload.name,
                "create_agent rejected: would exceed subagent depth cap",
            );
            self.write_parent_result(
                parent_session.as_ref(),
                ResultStatus::Rejected,
                None,
                None,
                Some(&format!(
                    "nested create_agent (max depth = {})",
                    self.deps.max_depth
                )),
            );
            return Ok(DeliveryActionOutput::default());
        }

        // 4. Hard depth gate, re-checked under the `spawned` lock to
        //    close the TOCTOU window: two concurrent calls from the
        //    same parent both observe step-3's soft check passing,
        //    both compute new_depth=N+1, both try to insert. We
        //    re-read the parent's depth here while holding the lock
        //    and bail if a concurrent winner has already pushed the
        //    parent deeper. The lock guards only the in-memory cache,
        //    so it isn't held across the DB writes that follow.
        let central = &self.deps.central;
        {
            let spawned = self
                .deps
                .spawned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let live_parent_depth = parent_session
                .as_ref()
                .and_then(|p| spawned.get(&p.agent_group_id).copied())
                .or(parent_depth);
            let Some(live_new_depth) = live_parent_depth.unwrap_or(0).checked_add(1) else {
                drop(spawned);
                warn!(
                    parent_depth = live_parent_depth.unwrap_or(0),
                    "create_agent rejected on lock re-check: parent depth saturated",
                );
                self.write_parent_result(
                    parent_session.as_ref(),
                    ResultStatus::Rejected,
                    None,
                    None,
                    Some("nested create_agent (parent depth saturated)"),
                );
                return Ok(DeliveryActionOutput::default());
            };
            if live_new_depth > self.deps.max_depth {
                drop(spawned);
                warn!(
                    live_parent_depth = live_parent_depth.unwrap_or(0),
                    max_depth = self.deps.max_depth,
                    "create_agent rejected on lock re-check: concurrent spawn won",
                );
                self.write_parent_result(
                    parent_session.as_ref(),
                    ResultStatus::Rejected,
                    None,
                    None,
                    Some(&format!(
                        "nested create_agent (max depth = {})",
                        self.deps.max_depth
                    )),
                );
                return Ok(DeliveryActionOutput::default());
            }
        }

        // 5. Central DB mutations: agent_groups → sessions → (optional)
        //    messaging_group_agents wiring.
        let group = agent_groups::create(
            central,
            CreateAgentGroup {
                name: payload.name.clone(),
                folder: payload.folder(),
                agent_provider: None,
            },
        )
        .map_err(|e| ModuleError::other("agent_to_agent", format!("agent_groups::create: {e}")))?;

        // Persist the new group's nesting depth so it survives host
        // restarts. The in-memory cache is also written so subsequent
        // gate checks within this process hit the cache.
        if let Err(err) = agent_groups::set_subagent_depth(central, group.id, new_depth) {
            warn!(
                agent_group = %group.id.as_uuid(),
                new_depth,
                ?err,
                "create_agent: set_subagent_depth failed; cap will not survive restart",
            );
        }

        // The instructions are not stored as a column on `agent_groups` —
        // production hosts persist them as `container_configs` or skill
        // files. TODO(team-ca): plumb instructions into container_configs
        // once the central schema lands a system_prompt column. For now we
        // surface them via the result row so the parent agent / operator
        // can hand-roll persistence.
        // Inherit the parent session's routing so the spawned agent
        // can reply into the same chat the parent is talking to. Without
        // this the child session lands with `messaging_group_id = NULL`
        // and its `send_message` calls have nowhere to deliver — the
        // failure mode that surfaced as "research agents I created
        // never reported back" on a live Telegram chat. The parent
        // wiring stays intact (parent still receives the user's
        // messages); we just copy the addressing onto the new session.
        // Falls back to None when there's no resolvable parent
        // (administrative / scripted `create_agent` calls).
        let (parent_messaging_group, parent_thread) = parent_session
            .as_ref()
            .and_then(|p| {
                sessions::get(central, p.session_id)
                    .ok()
                    .map(|s| (s.messaging_group_id, s.thread_id))
            })
            .unwrap_or((None, None));
        let session = sessions::create(
            central,
            CreateSession {
                agent_group_id: group.id,
                messaging_group_id: parent_messaging_group,
                thread_id: parent_thread,
                agent_provider: None,
            },
        )
        .map_err(|e| ModuleError::other("agent_to_agent", format!("sessions::create: {e}")))?;

        self.deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(group.id, new_depth);

        if let Some(channel) = payload.channel.as_deref() {
            if let Err(err) = self.create_wiring(group.id, channel, &payload.name) {
                warn!(
                    agent_group = %group.id.as_uuid(),
                    channel,
                    ?err,
                    "create_agent: wiring upsert failed; agent created but unwired",
                );
            }
        }

        info!(
            agent_group = %group.id.as_uuid(),
            session = %session.id.as_uuid(),
            name = %payload.name,
            "create_agent: spawned new agent group + session",
        );

        // 5. Notify the parent agent via its inbound.db.
        self.write_parent_result(
            parent_session.as_ref(),
            ResultStatus::Created,
            Some(session.id),
            Some(group.id),
            Some(payload.instructions.as_str()),
        );

        Ok(DeliveryActionOutput::default())
    }
}

/// Snapshot of the originating session — pulled from the central DB at
/// handler entry by matching the dispatch target's routing. Carrying both
/// ids lets `write_parent_result` open the right inbound.db.
#[derive(Debug, Clone, Copy)]
struct ParentSession {
    session_id: SessionId,
    agent_group_id: AgentGroupId,
}

impl CreateAgentHandler {
    /// Read the parent's recorded subagent depth. Tries the in-memory
    /// cache first, falls back to the persisted column. `None` means
    /// "no parent" or "parent has no recorded depth" (depth=0 root).
    fn lookup_parent_depth(&self, parent: Option<&ParentSession>) -> Option<u8> {
        let parent = parent?;
        {
            let spawned = self
                .deps
                .spawned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(d) = spawned.get(&parent.agent_group_id).copied() {
                return Some(d);
            }
        }
        match agent_groups::get_subagent_depth(&self.deps.central, parent.agent_group_id) {
            Ok(Some(d)) if d > 0 => {
                self.deps
                    .spawned
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(parent.agent_group_id, d);
                Some(d)
            }
            _ => None,
        }
    }

    /// Find the originating session from the dispatch target's routing.
    /// Tries, in order: `agent_group_id` (only set for `MessageKind::Agent`,
    /// not normally populated for system rows), then a lookup by
    /// `(channel_type, platform_id, thread_id)` via `messaging_groups` +
    /// `sessions`. Returns `None` when no match.
    ///
    /// TODO(team-ca): once `DeliveryActionInput` grows a `source_session_id`
    /// field (Team IP's domain — host-delivery service.rs), drop this
    /// best-effort lookup in favor of the direct id.
    fn resolve_parent(&self, input: &DeliveryActionInput) -> Option<ParentSession> {
        if let Some(ag) = input.target.agent_group_id {
            if let Ok(Some(sess)) = sessions::find_by_agent_group(&self.deps.central, ag) {
                return Some(ParentSession {
                    session_id: sess.id,
                    agent_group_id: sess.agent_group_id,
                });
            }
        }
        let channel_type = input.target.channel_type.as_ref()?;
        let platform_id = input.target.platform_id.as_deref()?;
        let mg = messaging_groups::get_by_platform(
            &self.deps.central,
            channel_type,
            platform_id,
        )
        .ok()
        .flatten()?;
        let thread = input.target.thread_id.as_deref();
        // Multiple agent groups may share one messaging_group; pick the most
        // recently active session for any agent wired to this mg. This is a
        // best-effort match; the TODO above is the real fix.
        let wirings = messaging_group_agents::list_for_mg(&self.deps.central, mg.id).ok()?;
        for w in wirings {
            if let Ok(Some(s)) = sessions::find_for_agent(
                &self.deps.central,
                w.agent_group_id,
                Some(mg.id),
                thread,
            ) {
                return Some(ParentSession {
                    session_id: s.id,
                    agent_group_id: s.agent_group_id,
                });
            }
        }
        None
    }

    /// Upsert a synthetic messaging-group + wiring so the new agent has a
    /// destination on the requested channel.
    fn create_wiring(
        &self,
        agent_group_id: AgentGroupId,
        channel: &str,
        agent_name: &str,
    ) -> Result<(), String> {
        let ct = ChannelType::new(channel);
        let platform_id = format!(
            "{SPAWN_PLATFORM_PREFIX}{}",
            agent_group_id.as_uuid().simple()
        );
        let mg = messaging_groups::upsert(
            &self.deps.central,
            UpsertMessagingGroup {
                channel_type: ct,
                platform_id,
                name: Some(format!("spawn:{agent_name}")),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .map_err(|e| e.to_string())?;
        messaging_group_agents::upsert(
            &self.deps.central,
            UpsertWiring {
                messaging_group_id: mg.id,
                agent_group_id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Append a `create_agent_result` system row to the parent session's
    /// inbound.db. The runner's `format_messages` will render this into the
    /// next turn's prompt as a `system:` line so the calling agent learns
    /// the real session / agent-group ids.
    fn write_parent_result(
        &self,
        parent: Option<&ParentSession>,
        status: ResultStatus,
        session_id: Option<SessionId>,
        agent_group_id: Option<AgentGroupId>,
        detail: Option<&str>,
    ) {
        let Some(parent) = parent else {
            info!(
                ?status,
                "create_agent_result: no parent session resolvable; skipping inbound notice",
            );
            return;
        };
        let paths = SessionPaths::new(
            &self.deps.data_root,
            parent.agent_group_id,
            parent.session_id,
        );
        let conn = match ironclaw_db::session::open_inbound(&paths) {
            Ok(c) => c,
            Err(err) => {
                warn!(?err, "create_agent_result: open_inbound failed; skipping");
                return;
            }
        };
        let mut body = serde_json::Map::new();
        body.insert("status".into(), serde_json::json!(status.as_str()));
        if let Some(sid) = session_id {
            body.insert(
                "session_id".into(),
                serde_json::json!(sid.as_uuid().to_string()),
            );
        }
        if let Some(agid) = agent_group_id {
            body.insert(
                "agent_group_id".into(),
                serde_json::json!(agid.as_uuid().to_string()),
            );
        }
        if let Some(d) = detail {
            body.insert("detail".into(), serde_json::json!(d));
        }
        let content = serde_json::json!({ "create_agent_result": body });
        let msg = WriteInbound {
            id: MessageId::new(),
            kind: MessageKind::System,
            timestamp: Utc::now(),
            content,
            trigger: false,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id: None,
        };
        if let Err(err) = messages_in::insert(&conn, &msg) {
            warn!(
                parent_session = %parent.session_id.as_uuid(),
                ?err,
                "create_agent_result: messages_in::insert failed; agent will not see result",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{DispatchTarget, MockModuleContext};
    use ironclaw_db::central::CentralDb;
    use ironclaw_db::session::SessionPaths;
    use ironclaw_db::tables::messages_in as messages_in_read;
    use ironclaw_types::{AgentGroupId, MessageKind, OutboundMessage};

    #[test]
    fn parses_simple_agent_name() {
        let r = parse("agent:helper").unwrap();
        assert_eq!(r.name, "helper");
        assert!(is_agent_destination("agent:helper"));
    }

    #[test]
    fn parses_url_form() {
        let r = parse("agent://my.bot").unwrap();
        assert_eq!(r.name, "my.bot");
    }

    #[test]
    fn allows_dash_underscore_dot_in_name() {
        assert_eq!(parse("agent:foo-bar_baz.42").unwrap().name, "foo-bar_baz.42");
    }

    #[test]
    fn rejects_empty_name() {
        assert!(parse("agent:").is_none());
        assert!(parse("agent://").is_none());
    }

    #[test]
    fn rejects_invalid_chars() {
        assert!(parse("agent:hello world").is_none());
        assert!(parse("agent:hello/etc").is_none());
        assert!(parse("agent:hello!").is_none());
    }

    #[test]
    fn rejects_non_agent_strings() {
        assert!(parse("telegram:chat-1").is_none());
        assert!(parse("helper").is_none());
        assert!(parse("").is_none());
        assert!(!is_agent_destination("https://example.com"));
    }

    #[test]
    fn parses_trimmed_input() {
        let r = parse("  agent:helper  ").unwrap();
        assert_eq!(r.name, "helper");
    }

    #[test]
    fn agent_ref_serde_roundtrip() {
        let r = AgentRef {
            name: "helper".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: AgentRef = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[tokio::test]
    async fn install_registers_interceptor() {
        let m = AgentToAgentModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert_eq!(ctx.registered(), vec!["message_interceptor"]);
    }

    #[tokio::test]
    async fn create_agent_module_registers_action() {
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let m = CreateAgentModule::new(central, tmp.path().to_path_buf(), always_allow());
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let actions = ctx.delivery_actions();
        assert_eq!(actions, vec!["create_agent"]);
    }

    #[tokio::test]
    async fn interceptor_is_passthrough_for_agent_channel() {
        let m = AgentToAgentModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let hook = ctx.interceptors.lock().unwrap()[0].clone();
        let dec = hook(InterceptorCtx {
            message: OutboundMessage {
                kind: MessageKind::Agent,
                content: serde_json::json!({}),
                files: vec![],
            },
            channel_type: Some(ChannelType::new(ChannelType::AGENT)),
            platform_id: None,
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        });
        assert!(dec.is_passthrough());
    }

    #[tokio::test]
    async fn interceptor_is_passthrough_for_non_agent() {
        let m = AgentToAgentModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let hook = ctx.interceptors.lock().unwrap()[0].clone();
        let dec = hook(InterceptorCtx {
            message: OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({}),
                files: vec![],
            },
            channel_type: Some(ChannelType::new("telegram")),
            platform_id: Some("C1".into()),
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        });
        assert!(dec.is_passthrough());
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(AgentToAgentModule.name(), "agent_to_agent");
    }

    #[test]
    fn create_agent_module_name_is_stable() {
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let m = CreateAgentModule::new(central, tmp.path().to_path_buf(), always_allow());
        assert_eq!(m.name(), "create_agent");
    }

    // Compile-time use of DispatchTarget::agent to keep its tests honest.
    #[test]
    fn dispatch_target_agent_used() {
        let t = DispatchTarget::agent(AgentGroupId::new());
        assert_eq!(
            t.channel_type.as_ref().map(ChannelType::as_str),
            Some(ChannelType::AGENT)
        );
    }

    // -----------------------------------------------------------------------
    // CreateAgentHandler tests
    // -----------------------------------------------------------------------

    /// Build a handler + parent session that the handler can resolve via the
    /// dispatch target's `(channel_type, platform_id)`. Returns the handler,
    /// the central DB, the data-root tempdir, and the parent session ids.
    fn make_handler(
        permission: CreateAgentPermissionCheck,
    ) -> (
        CreateAgentHandler,
        CentralDb,
        tempfile::TempDir,
        ParentSession,
        DispatchTarget,
    ) {
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();

        // Seed a "parent" agent group + messaging group + wiring + session
        // so the handler's lookup can find it.
        let parent_ag = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "parent".into(),
                folder: "parent".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = messaging_groups::upsert(
            &central,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("cli"),
                platform_id: "stdin".into(),
                name: Some("parent-cli".into()),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        messaging_group_agents::upsert(
            &central,
            UpsertWiring {
                messaging_group_id: mg.id,
                agent_group_id: parent_ag.id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();
        let parent_session = sessions::create(
            &central,
            CreateSession {
                agent_group_id: parent_ag.id,
                messaging_group_id: Some(mg.id),
                thread_id: None,
                agent_provider: None,
            },
        )
        .unwrap();

        // Pre-create the parent's inbound.db so `messages_in::insert` works.
        let paths = SessionPaths::new(tmp.path(), parent_ag.id, parent_session.id);
        ironclaw_db::session::open_inbound(&paths).unwrap();

        let module = CreateAgentModule::new(
            central.clone(),
            tmp.path().to_path_buf(),
            permission,
        );
        let deps = module.deps().clone();
        let handler = CreateAgentHandler { deps };
        let parent = ParentSession {
            session_id: parent_session.id,
            agent_group_id: parent_ag.id,
        };
        let target = DispatchTarget::channel(
            ChannelType::new("cli"),
            "stdin".into(),
            None,
        );
        (handler, central, tmp, parent, target)
    }

    fn read_inbound_create_results(
        data_root: &std::path::Path,
        parent: ParentSession,
    ) -> Vec<serde_json::Value> {
        let paths = SessionPaths::new(data_root, parent.agent_group_id, parent.session_id);
        let conn = ironclaw_db::session::open_inbound(&paths).unwrap();
        let pending = messages_in_read::get_pending(&conn, true, 100).unwrap();
        pending
            .into_iter()
            .filter_map(|row| {
                let v = &row.content;
                if v.get("create_agent_result").is_some() {
                    Some(v.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn create_agent_inserts_agent_group_and_session() {
        let (handler, central, _tmp, _parent, target) = make_handler(always_allow());
        let before_ag = agent_groups::list(&central).unwrap().len();
        let before_sess = sessions::list_active(&central).unwrap().len();

        let out = handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "research-bot",
                    "instructions": "Investigate things",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        assert!(out.message.is_none(), "create_agent has no chat reply");

        let after_ag = agent_groups::list(&central).unwrap();
        let after_sess = sessions::list_active(&central).unwrap();
        assert_eq!(after_ag.len(), before_ag + 1, "one new agent_groups row");
        assert_eq!(after_sess.len(), before_sess + 1, "one new sessions row");
        let new_group = after_ag
            .iter()
            .find(|g| g.name == "research-bot")
            .expect("named agent present");
        assert!(
            new_group.folder.starts_with("research-bot-"),
            "folder slug derived from name (got {})",
            new_group.folder,
        );
    }

    #[test]
    fn child_session_inherits_parent_messaging_group() {
        // Regression for the live Telegram failure: child sessions used
        // to land with `messaging_group_id = NULL`, so `send_message`
        // from the child had no return path. Now create_agent copies
        // the parent session's routing onto the new session.
        let (handler, central, _tmp, parent, target) = make_handler(always_allow());
        let before_sessions: std::collections::HashSet<SessionId> =
            sessions::list_active(&central).unwrap().iter().map(|s| s.id).collect();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "scout",
                    "instructions": "go look",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let new_session = sessions::list_active(&central)
            .unwrap()
            .into_iter()
            .find(|s| !before_sessions.contains(&s.id))
            .expect("a new session was created");
        let parent_session = sessions::get(&central, parent.session_id).unwrap();
        assert!(
            parent_session.messaging_group_id.is_some(),
            "test parent must have a wired messaging group"
        );
        assert_eq!(
            new_session.messaging_group_id, parent_session.messaging_group_id,
            "child must inherit parent's messaging_group_id, not land with NULL"
        );
        // thread_id propagates too (None in this test fixture; the
        // important behaviour is that they match the parent).
        assert_eq!(
            new_session.thread_id, parent_session.thread_id,
            "child must inherit parent's thread_id"
        );
    }

    #[test]
    fn create_agent_emits_result_to_parent_inbound() {
        let (handler, _central, tmp, parent, target) = make_handler(always_allow());
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "child",
                    "instructions": "do work",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1, "exactly one result row landed in parent");
        let r = &results[0]["create_agent_result"];
        assert_eq!(r["status"], "created");
        assert!(r["session_id"].is_string(), "real session id present");
        assert!(r["agent_group_id"].is_string(), "real agent group id present");
    }

    #[test]
    fn create_agent_with_channel_creates_wiring() {
        let (handler, central, _tmp, _parent, target) = make_handler(always_allow());
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "wired",
                    "instructions": "wired up",
                    "channel": "cli",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let groups = agent_groups::list(&central).unwrap();
        let new_ag = groups.iter().find(|g| g.name == "wired").unwrap();
        let wirings = messaging_group_agents::list_for_ag(&central, new_ag.id).unwrap();
        assert_eq!(wirings.len(), 1, "exactly one wiring for the new agent");
        let mg = messaging_groups::get(&central, wirings[0].messaging_group_id).unwrap();
        assert_eq!(mg.channel_type.as_str(), "cli");
        assert!(
            mg.platform_id.starts_with(SPAWN_PLATFORM_PREFIX),
            "platform_id is the synthetic spawn placeholder (got {})",
            mg.platform_id,
        );
    }

    #[test]
    fn create_agent_denied_when_permission_missing() {
        let (handler, central, tmp, parent, target) = make_handler(always_deny());
        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "blocked",
                    "instructions": "should not exist",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before, "denied request must not create rows");
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "denied");
    }

    #[test]
    fn create_agent_refuses_when_max_depth_exceeded() {
        // Default cap is DEFAULT_MAX_SUBAGENT_DEPTH (3); a parent
        // already at depth 3 spawning would land the child at depth 4,
        // which exceeds the cap.
        let (handler, central, tmp, parent, target) = make_handler(always_allow());
        handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.agent_group_id, DEFAULT_MAX_SUBAGENT_DEPTH);
        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "great-grandchild",
                    "instructions": "would exceed depth cap",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before, "nested request must not create rows");
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "rejected");
        // The rejection message references the current cap so the
        // model can self-correct without guessing.
        let detail = results[0]["create_agent_result"]["detail"]
            .as_str()
            .unwrap_or_default();
        assert!(
            detail.contains(&format!("max depth = {DEFAULT_MAX_SUBAGENT_DEPTH}")),
            "expected cap mention in detail, got: {detail}"
        );
    }

    #[test]
    fn create_agent_allows_intermediate_depths_under_default_cap() {
        // Parent at depth 2 (still under the default cap of 3) spawning
        // is allowed; the new group is recorded at depth 3.
        let (handler, central, _tmp, parent, target) = make_handler(always_allow());
        handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.agent_group_id, 2);
        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "depth-three-child",
                    "instructions": "depth 3 is fine",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before + 1, "depth-3 spawn must create the group");
        // The new group should be tracked at depth 3 so a further spawn
        // from it would be the one that fails.
        let depths: Vec<u8> = handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .copied()
            .collect();
        assert!(depths.contains(&3));
    }

    #[test]
    fn create_agent_with_max_depth_one_reproduces_historical_behaviour() {
        // Pin `max_depth = 1` on the handler. Then a parent recorded at
        // depth 1 spawning would land the child at depth 2 — rejected.
        let (mut handler, central, tmp, parent, target) = make_handler(always_allow());
        handler.deps.max_depth = 1;
        handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.agent_group_id, 1);
        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "grandchild",
                    "instructions": "would be depth 2",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before, "max_depth=1 must reject depth-2 children");
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "rejected");
    }

    #[test]
    fn with_max_depth_clamps_zero_to_one() {
        // `with_max_depth(0)` would otherwise reject every spawn; the
        // builder clamps to 1 so the gate stays useful.
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let module = CreateAgentModule::new(central, tmp.path().to_path_buf(), always_allow())
            .with_max_depth(0);
        assert_eq!(module.deps().max_depth, 1);
    }

    #[test]
    fn create_agent_invalid_payload_surfaces_back() {
        let (handler, central, tmp, parent, target) = make_handler(always_allow());
        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({"name": "no-instructions"}),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before, "invalid payload must not create rows");
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "invalid");
    }

    /// `users_table_check` denies when the install has no roles
    /// granted. This is the bootstrap-safe default — without it any
    /// untrusted operator could spawn agents the moment the host
    /// boots.
    #[test]
    fn users_table_check_denies_on_empty_install() {
        use ironclaw_db::tables::users::{self, UpsertUser};
        let central = CentralDb::open_in_memory().unwrap();
        // Even with users present, no roles means deny.
        users::upsert(
            &central,
            UpsertUser {
                kind: "telegram".into(),
                identity: "1".into(),
                display_name: Some("op".into()),
            },
        )
        .unwrap();
        let check = users_table_check(central);
        let ctx = CreateAgentPermissionCtx {
            parent_agent_group_id: None,
            parent_session_id: None,
            requested_name: "x".into(),
        };
        assert!(!check(&ctx), "no roles granted → deny");
    }

    /// Granting global Owner opens the gate for every parent.
    #[test]
    fn users_table_check_allows_when_global_owner_exists() {
        use ironclaw_db::tables::users::{self, UpsertUser};
        let central = CentralDb::open_in_memory().unwrap();
        let user = users::upsert(
            &central,
            UpsertUser {
                kind: "telegram".into(),
                identity: "1".into(),
                display_name: Some("op".into()),
            },
        )
        .unwrap();
        user_roles::grant(&central, user.id, user_roles::Role::Owner, None, None).unwrap();
        let check = users_table_check(central);
        let ctx = CreateAgentPermissionCtx {
            parent_agent_group_id: Some(AgentGroupId::new()),
            parent_session_id: None,
            requested_name: "x".into(),
        };
        assert!(check(&ctx), "global Owner → allow");
    }

    /// Group-scoped Admin opens the gate only for that scope.
    #[test]
    fn users_table_check_allows_only_for_scoped_admin_when_no_global() {
        use ironclaw_db::tables::users::{self, UpsertUser};
        let central = CentralDb::open_in_memory().unwrap();
        let user = users::upsert(
            &central,
            UpsertUser {
                kind: "telegram".into(),
                identity: "2".into(),
                display_name: Some("scoped-op".into()),
            },
        )
        .unwrap();
        let scoped_group = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "scoped".into(),
                folder: "scoped".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let scoped_ag = scoped_group.id;
        user_roles::grant(
            &central,
            user.id,
            user_roles::Role::Admin,
            Some(scoped_ag),
            None,
        )
        .unwrap();
        let check = users_table_check(central);
        // In-scope: allow.
        let in_scope = CreateAgentPermissionCtx {
            parent_agent_group_id: Some(scoped_ag),
            parent_session_id: None,
            requested_name: "x".into(),
        };
        assert!(check(&in_scope), "scoped Admin within own group → allow");
        // Out-of-scope: deny.
        let out_of_scope = CreateAgentPermissionCtx {
            parent_agent_group_id: Some(AgentGroupId::new()),
            parent_session_id: None,
            requested_name: "x".into(),
        };
        assert!(
            !check(&out_of_scope),
            "scoped Admin must not leak to other groups"
        );
        // Orphan parent (no scope to match): deny.
        let orphan = CreateAgentPermissionCtx {
            parent_agent_group_id: None,
            parent_session_id: None,
            requested_name: "x".into(),
        };
        assert!(!check(&orphan), "no parent scope → cannot match scoped grant");
    }

    // -----------------------------------------------------------------------
    // Code-review fixes — depth-cap TOCTOU, restart persistence,
    // saturation, poison handling, orphan-warn.
    // -----------------------------------------------------------------------

    /// Finding 4 (TOCTOU): cap is re-checked under the `spawned` lock.
    #[test]
    fn create_agent_depth_recheck_under_lock_catches_concurrent_winner() {
        let (mut handler, central, tmp, parent, target) = make_handler(always_allow());
        handler.deps.max_depth = 2;
        // Soft check sees parent at depth 1 -> new_depth = 2 -> allowed.
        handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.agent_group_id, 1);
        // Simulate a concurrent winner: persist parent at depth 2 in
        // the DB. The hard re-check pulls from the cache first, so we
        // mutate the cache too. We can't actually race threads here,
        // so we model the cache-state the loser would observe at lock
        // acquire time.
        agent_groups::set_subagent_depth(&central, parent.agent_group_id, 2).unwrap();
        handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.agent_group_id, 2);

        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "loser",
                    "instructions": "raced for the slot",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(
            after, before,
            "racing loser must not create rows once cache shows the winner"
        );
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "rejected");
    }

    /// Finding 5 (persistence): depth cap survives module reconstruction.
    #[test]
    fn depth_cap_survives_module_reconstruction() {
        let (handler, central, tmp, parent, target) = make_handler(always_allow());
        // Persist parent at the cap directly via the DB (no cache).
        agent_groups::set_subagent_depth(
            &central,
            parent.agent_group_id,
            DEFAULT_MAX_SUBAGENT_DEPTH,
        )
        .unwrap();

        // "Restart": new module, fresh in-memory cache.
        let module = CreateAgentModule::new(
            central.clone(),
            tmp.path().to_path_buf(),
            always_allow(),
        );
        let fresh = CreateAgentHandler {
            deps: module.deps().clone(),
        };
        assert!(
            fresh
                .deps
                .spawned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "freshly constructed handler must start with empty cache",
        );
        let _ = handler; // silence unused warning; kept for setup symmetry.

        let before = agent_groups::list(&central).unwrap().len();
        fresh
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "post-restart-child",
                    "instructions": "must be rejected",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(
            after, before,
            "persisted parent depth must still gate after restart",
        );
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "rejected");
    }

    /// Finding 9 (saturation): `with_max_depth` clamps at the ceiling.
    #[test]
    fn with_max_depth_clamps_above_ceiling() {
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let module = CreateAgentModule::new(central, tmp.path().to_path_buf(), always_allow())
            .with_max_depth(u8::MAX);
        assert_eq!(module.deps().max_depth, MAX_SUBAGENT_DEPTH_CEILING);
    }

    /// Finding 9 (saturation): boundary check at `max_depth` rejects.
    #[test]
    fn create_agent_rejects_at_max_depth_boundary() {
        let (mut handler, central, tmp, parent, target) = make_handler(always_allow());
        handler.deps.max_depth = MAX_SUBAGENT_DEPTH_CEILING;
        handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.agent_group_id, MAX_SUBAGENT_DEPTH_CEILING);
        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "boundary-child",
                    "instructions": "at the cap, child would exceed",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before);
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results[0]["create_agent_result"]["status"], "rejected");
    }

    /// Finding 9 (saturation): `checked_add` rejection at `u8::MAX` parent.
    #[test]
    fn create_agent_rejects_when_checked_add_overflows() {
        let (mut handler, central, tmp, parent, target) = make_handler(always_allow());
        handler.deps.max_depth = u8::MAX;
        handler
            .deps
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(parent.agent_group_id, u8::MAX);
        let before = agent_groups::list(&central).unwrap().len();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "would-saturate",
                    "instructions": "parent at u8::MAX",
                }),
                target,
                session_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before, "saturated-parent spawn must be rejected");
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "rejected");
        let detail = results[0]["create_agent_result"]["detail"]
            .as_str()
            .unwrap_or_default();
        assert!(
            detail.contains("saturated"),
            "expected saturation explanation, got: {detail}"
        );
    }

    /// Finding 12 (orphan-warn): orphan depth-cap rejection still emits a warn.
    #[test]
    fn orphan_depth_cap_rejection_emits_warn() {
        use std::sync::{Arc, Mutex as StdMutex};
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct CaptureWriter(Arc<StdMutex<Vec<u8>>>);
        impl std::io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for CaptureWriter {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf: Arc<StdMutex<Vec<u8>>> = Arc::new(StdMutex::new(Vec::new()));
        let writer = CaptureWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();

        // Pin max_depth=0 + unresolvable target so the orphan branch
        // of the depth gate fires.
        let (mut handler, _central, _tmp, _parent, _target) = make_handler(always_allow());
        handler.deps.max_depth = 0;
        let orphan_target = DispatchTarget::channel(
            ChannelType::new("nonexistent"),
            "no-such-platform".into(),
            None,
        );

        tracing::subscriber::with_default(subscriber, || {
            handler
                .handle(DeliveryActionInput {
                    action: "create_agent".into(),
                    payload: serde_json::json!({
                        "name": "orphan",
                        "instructions": "no parent resolvable",
                    }),
                    target: orphan_target,
                    session_id: None,
                })
                .unwrap();
        });

        let captured = String::from_utf8(
            buf.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .expect("captured warn output is utf-8");
        assert!(
            captured.contains("create_agent rejected"),
            "expected a warn line on orphan depth-cap rejection, got: {captured}",
        );
    }
}
