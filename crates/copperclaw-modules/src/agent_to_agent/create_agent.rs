//! The `create_agent` delivery action and its handler.
//!
//! When an agent calls the `create_agent` MCP tool, the runner writes a
//! `kind=system` outbound row with content `{"create_agent": {"name":
//! "...", "instructions": "...", "channel": "..."}}`. The host's
//! delivery loop parses the action name and dispatches to
//! [`CreateAgentHandler::handle`], which:
//!
//! a. Permission-gates via the configured closure (defaults to deny so
//!    production wiring must opt in).
//! b. Refuses if accepting the request would push the new group past
//!    the configured subagent-depth cap (default
//!    [`DEFAULT_MAX_SUBAGENT_DEPTH`]). Depth = parent's depth + 1 (or 1
//!    when the parent is itself an un-spawned agent, e.g. the initial
//!    agent in the install).
//! c. INSERTs `agent_groups` + `sessions` (+ optional
//!    `messaging_group_agents`).
//! d. Writes a `create_agent_result` system row into the *parent*
//!    session's `inbound.db` so the calling agent sees the real id on
//!    its next turn (see [`super::inbound_seed`] for the inbound write
//!    helpers).
//!
//! The container manager's reconcile loop polls the `sessions` table on
//! a short timer, so the new agent's container will spawn on its next
//! tick without any explicit notification from the handler.

use super::SPAWN_PLATFORM_PREFIX;
use super::depth::{DEFAULT_MAX_SUBAGENT_DEPTH, MAX_SUBAGENT_DEPTH_CEILING};
use super::permissions::{CreateAgentPermissionCheck, CreateAgentPermissionCtx};
use crate::context::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput, Module, ModuleContext,
};
use crate::error::ModuleError;
use async_trait::async_trait;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{
    agent_groups::{self, CreateAgentGroup},
    container_configs::{self, UpsertContainerConfig},
    messaging_group_agents::{self, UpsertWiring},
    messaging_groups::{self, UpsertMessagingGroup},
    sessions::{self, CreateSession},
};
use copperclaw_types::{AgentGroupId, ChannelType, EngageMode, SessionId, SessionMode};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Companion module that registers the `create_agent` delivery action. The
/// handler runs in-process against the central DB, so this module is the
/// natural place to plumb DB handles into the host's hook surface.
///
/// Installed alongside `AgentToAgentModule` in `boot::install_modules` —
/// the two modules are intentionally separate: the unit-struct
/// `AgentToAgentModule` is an interceptor only, this one carries the DB
/// state needed by the `create_agent` handler.
pub struct CreateAgentModule {
    pub(super) deps: HandlerDeps,
}

#[derive(Clone)]
pub(super) struct HandlerDeps {
    pub(super) central: CentralDb,
    pub(super) data_root: PathBuf,
    pub(super) permission_check: CreateAgentPermissionCheck,
    /// Mutex held across the hard depth re-check + the central-DB
    /// insert. Closes the TOCTOU window between the soft check and the
    /// commit: two concurrent calls from the same parent both pass the
    /// soft check, but only one acquires this lock and re-reads the
    /// parent's depth from the DB before deciding to insert. The lock
    /// holds NO per-agent state — depth itself lives in
    /// `agent_groups.subagent_depth` so it survives host restarts and
    /// stays correct when an `AgentGroupId` is deleted and a future
    /// group reuses the slot. This used to be a
    /// `HashMap<AgentGroupId, u8>` write-through accelerator, but on a
    /// long-running host with many short-lived groups that map grew
    /// without bound and could return stale depths for reused ids.
    /// The DB hit per `create_agent` is one extra SELECT — not a hot
    /// path (spawns are operator-driven, not per-message).
    pub(super) depth_gate: Arc<Mutex<()>>,
    /// Hard cap on subagent depth — see [`DEFAULT_MAX_SUBAGENT_DEPTH`].
    /// `1` reproduces the historical "no nested spawns at all" rule.
    pub(super) max_depth: u8,
}

impl CreateAgentModule {
    /// Build a module with the `create_agent` delivery action wired up.
    ///
    /// `central` is the host's central DB (where `agent_groups`, `sessions`,
    /// `messaging_group_agents` live). `data_root` is the on-disk root that
    /// `SessionPaths::new` walks to find each session's `inbound.db`.
    ///
    /// `permission_check` is consulted at every `handle()` call. Pass
    /// [`super::always_allow`] for tests and a `users`-table lookup in
    /// production.
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
                depth_gate: Arc::new(Mutex::new(())),
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
    /// central DB or override `max_depth`.
    #[cfg(test)]
    pub(super) fn deps(&self) -> &HandlerDeps {
        &self.deps
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
    pub(super) deps: HandlerDeps,
}

/// Outcome of a single `create_agent` invocation. The handler writes a JSON
/// rendering of this back to the parent session's inbound.db as a `system`
/// row so the parent agent can see the real ids on its next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResultStatus {
    Created,
    Denied,
    Rejected,
    Invalid,
}

impl ResultStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Denied => "denied",
            Self::Rejected => "rejected",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CreateAgentPayload {
    pub(super) name: String,
    pub(super) instructions: String,
    pub(super) channel: Option<String>,
}

impl CreateAgentPayload {
    pub(super) fn parse(v: &serde_json::Value) -> Result<Self, String> {
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
    pub(super) fn folder(&self) -> String {
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
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
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
        //    DB writes. The authoritative re-check happens under
        //    `depth_gate` at insert time (step 4) to close the TOCTOU
        //    race between concurrent calls from the same parent.
        //    Parent depth comes straight from the central DB
        //    (`agent_groups.subagent_depth`), so the gate survives
        //    host restarts and is correct when an `AgentGroupId` is
        //    deleted and a later group reuses it.
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

        // 4. Hard depth gate, re-checked while holding `depth_gate` to
        //    close the TOCTOU window: two concurrent calls from the
        //    same parent both observe step-3's soft check passing,
        //    both compute new_depth=N+1, both try to insert. The
        //    winner advances `agent_groups.subagent_depth`; the loser
        //    re-reads the DB under this lock and bails. The lock is
        //    process-wide rather than per-parent because
        //    `create_agent` is operator-driven (not the message hot
        //    path) — the marginal lock contention is irrelevant, and
        //    a single lock means we never grow a per-AgentGroupId map.
        //    The lock is held across `agent_groups::create` +
        //    `set_subagent_depth` further down so the loser's re-read
        //    here sees the winner's commit.
        let central = &self.deps.central;
        let _depth_lock = self
            .deps
            .depth_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live_parent_depth = self.lookup_parent_depth(parent_session.as_ref());
        let Some(live_new_depth) = live_parent_depth.unwrap_or(0).checked_add(1) else {
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
        // restarts. We use `live_new_depth` (the value re-derived
        // under `depth_gate`) rather than the soft-check `new_depth`
        // so a concurrent winner that bumped the parent between the
        // two checks can't cause us to under-record the child's depth.
        if let Err(err) = agent_groups::set_subagent_depth(central, group.id, live_new_depth) {
            warn!(
                agent_group = %group.id.as_uuid(),
                new_depth = live_new_depth,
                ?err,
                "create_agent: set_subagent_depth failed; cap will not survive restart",
            );
        }

        // Inherit the parent group's container config (model, provider,
        // effort, skills, packages, image) onto the child. Without this the
        // child group has no `container_configs` row, so the container
        // manager falls back to the host's `COPPERCLAW_DEFAULT_MODEL` — which
        // silently downgrades every sibling to the host default regardless of
        // what model the spawning parent runs on. Surfaced live: a parent on
        // a capable model (qwen) spawned six builders that all booted on the
        // weak host-default local model and produced nothing. No-op when
        // there's no resolvable parent (scripted calls) or the parent itself
        // has no config row (then the child shares the parent's host-default
        // behaviour, which is correct).
        if let Some(parent) = parent_session.as_ref() {
            self.inherit_parent_container_config(parent.agent_group_id, group.id);
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
        // Persist the parent → child link so the runtime can route the
        // child's default `send_message` (no explicit `to:`) back into the
        // parent's `inbound.db` instead of dumping it into the user's chat.
        // See docs/plans/agent-to-agent-routing.md Phase 1.
        let source_session_id = parent_session.as_ref().map(|p| p.session_id);
        let session = sessions::create(
            central,
            CreateSession {
                agent_group_id: group.id,
                messaging_group_id: parent_messaging_group,
                thread_id: parent_thread,
                agent_provider: None,
                source_session_id,
            },
        )
        .map_err(|e| ModuleError::other("agent_to_agent", format!("sessions::create: {e}")))?;

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

        // Copy the parent's `session_routing` (channel_type +
        // platform_id + thread_id) into the child's inbound.db. The
        // delivery service uses this record — not `sessions.messaging_
        // group_id` — to resolve where to send outbound chat messages,
        // so without it the child's `send_message` calls fail with
        // `NoRoute` and the user never sees the reply. The router
        // normally writes session_routing on inbound arrival; spawned
        // sessions have no inbound event so we mirror it here.
        if let Some(parent) = parent_session.as_ref() {
            self.copy_parent_session_routing(
                parent.agent_group_id,
                parent.session_id,
                group.id,
                session.id,
            );
        }

        // Seed the child's inbound.db with the operator's instructions
        // as the first user-side chat message. Without this kicker the
        // child has zero pending inbound, the container manager never
        // spawns it, and the spawned agent sits inert forever. Surfaced
        // live as "I created three research agents and none of them
        // ever reported back" — the agents existed in the central DB
        // but their containers were never started because the
        // `payload.instructions` text was returned to the parent in the
        // create_agent_result row and lost from there.
        // Look up the parent agent group's display name so we can tell
        // the child how to address replies back to its parent for
        // consolidation, rather than dumping findings directly into the
        // user's chat. None if the parent's row isn't resolvable
        // (administrative invocations) — in that case the child gets a
        // softer "wiring will route your reply back" instruction.
        let parent_name = parent_session.as_ref().and_then(|p| {
            agent_groups::get(central, p.agent_group_id)
                .ok()
                .map(|g| g.name)
        });
        self.seed_child_inbound(
            group.id,
            session.id,
            &payload.name,
            parent_name.as_deref(),
            &payload.instructions,
        );

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
pub(super) struct ParentSession {
    pub(super) session_id: SessionId,
    pub(super) agent_group_id: AgentGroupId,
}

impl CreateAgentHandler {
    /// Read the parent's recorded subagent depth straight from the
    /// central DB. `None` means "no parent" or "parent has no recorded
    /// depth" (depth=0 root). There is no in-memory cache: depth is
    /// only consulted on `create_agent`, which is an operator-driven
    /// action — not the message hot path — so a SELECT per call is
    /// cheap, and dropping the cache eliminates a memory leak (one
    /// entry per ever-spawned agent group) plus a correctness bug (a
    /// stale entry for a deleted-then-reused `AgentGroupId`).
    pub(super) fn lookup_parent_depth(&self, parent: Option<&ParentSession>) -> Option<u8> {
        let parent = parent?;
        match agent_groups::get_subagent_depth(&self.deps.central, parent.agent_group_id) {
            Ok(Some(d)) if d > 0 => Some(d),
            _ => None,
        }
    }

    /// Copy the parent group's `container_configs` row onto the freshly
    /// created child group so the child inherits the parent's model,
    /// provider, effort, skills, packages, and image instead of silently
    /// falling back to the host's `COPPERCLAW_DEFAULT_MODEL`.
    ///
    /// `config_fingerprint` only hashes image-relevant fields (packages /
    /// skills / mcp — see [`container_configs::compute_fingerprint`]), so
    /// copying it verbatim alongside `image_tag` lets the child adopt the
    /// parent's existing image with no rebuild. `assistant_name` is the one
    /// field deliberately NOT inherited — that's the parent's own identity;
    /// the child uses its own group name.
    ///
    /// Best-effort: a failure to read or write is logged, not fatal. The
    /// child then falls back to host defaults — the pre-fix behaviour — so
    /// the spawn still succeeds.
    fn inherit_parent_container_config(
        &self,
        parent_group: AgentGroupId,
        child_group: AgentGroupId,
    ) {
        let central = &self.deps.central;
        let cfg = match container_configs::get(central, parent_group) {
            Ok(Some(cfg)) => cfg,
            // Parent runs on host defaults — nothing to copy; the child
            // inherits the same default behaviour, which is correct.
            Ok(None) => return,
            Err(err) => {
                warn!(
                    parent_group = %parent_group.as_uuid(),
                    child_group = %child_group.as_uuid(),
                    ?err,
                    "create_agent: read parent container config failed; child falls back to host defaults",
                );
                return;
            }
        };
        let req = UpsertContainerConfig {
            agent_group_id: child_group,
            provider: cfg.provider,
            model: cfg.model,
            effort: cfg.effort,
            image_tag: cfg.image_tag,
            // Deliberately not inherited: the child has its own identity.
            assistant_name: None,
            max_messages_per_prompt: cfg.max_messages_per_prompt,
            skills: cfg.skills,
            mcp_servers: cfg.mcp_servers,
            packages_apt: cfg.packages_apt,
            packages_npm: cfg.packages_npm,
            additional_mounts: cfg.additional_mounts,
            cli_scope: cfg.cli_scope,
            config_fingerprint: cfg.config_fingerprint,
            egress_allow: cfg.egress_allow,
            resource_limits: cfg.resource_limits,
            coding_enabled: cfg.coding_enabled,
            surface_thinking: cfg.surface_thinking,
            // Inherit the parent's tool-profile so a child agent stays
            // within the same tool-authorization scope as its parent.
            tool_profile: cfg.tool_profile,
        };
        if let Err(err) = container_configs::upsert(central, req) {
            warn!(
                parent_group = %parent_group.as_uuid(),
                child_group = %child_group.as_uuid(),
                ?err,
                "create_agent: copy parent container config to child failed; child falls back to host defaults",
            );
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
    pub(super) fn resolve_parent(&self, input: &DeliveryActionInput) -> Option<ParentSession> {
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
        let mg = messaging_groups::get_by_platform(&self.deps.central, channel_type, platform_id)
            .ok()
            .flatten()?;
        let thread = input.target.thread_id.as_deref();
        // Multiple agent groups may share one messaging_group; pick the most
        // recently active session for any agent wired to this mg. This is a
        // best-effort match; the TODO above is the real fix.
        let wirings = messaging_group_agents::list_for_mg(&self.deps.central, mg.id).ok()?;
        for w in wirings {
            if let Ok(Some(s)) =
                sessions::find_for_agent(&self.deps.central, w.agent_group_id, Some(mg.id), thread)
            {
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
    pub(super) fn create_wiring(
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
}

#[cfg(test)]
mod tests {
    use super::super::permissions::{always_allow, always_deny};
    use super::*;
    use crate::context::{DispatchTarget, MockModuleContext};
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::session::SessionPaths;
    use copperclaw_db::tables::messages_in as messages_in_read;
    use copperclaw_types::{MessageKind, SessionId};

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

    #[test]
    fn create_agent_module_name_is_stable() {
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let m = CreateAgentModule::new(central, tmp.path().to_path_buf(), always_allow());
        assert_eq!(m.name(), "create_agent");
    }

    // -----------------------------------------------------------------------
    // CreateAgentHandler tests
    // -----------------------------------------------------------------------

    /// Build a handler + parent session that the handler can resolve via the
    /// dispatch target's `(channel_type, platform_id)`. Returns the handler,
    /// the central DB, the data-root tempdir, and the parent session ids.
    pub(super) fn make_handler(
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
                source_session_id: None,
            },
        )
        .unwrap();

        // Pre-create the parent's inbound.db so `messages_in::insert` works.
        let paths = SessionPaths::new(tmp.path(), parent_ag.id, parent_session.id);
        copperclaw_db::session::open_inbound(&paths).unwrap();

        let module = CreateAgentModule::new(central.clone(), tmp.path().to_path_buf(), permission);
        let deps = module.deps().clone();
        let handler = CreateAgentHandler { deps };
        let parent = ParentSession {
            session_id: parent_session.id,
            agent_group_id: parent_ag.id,
        };
        let target = DispatchTarget::channel(ChannelType::new("cli"), "stdin".into(), None);
        (handler, central, tmp, parent, target)
    }

    pub(super) fn read_inbound_create_results(
        data_root: &std::path::Path,
        parent: ParentSession,
    ) -> Vec<serde_json::Value> {
        let paths = SessionPaths::new(data_root, parent.agent_group_id, parent.session_id);
        let conn = copperclaw_db::session::open_inbound(&paths).unwrap();
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
                row_id: None,
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
    fn child_inherits_parent_container_config() {
        // Regression for the silent model-downgrade: a child spawned by
        // create_agent must inherit the parent group's model / provider /
        // effort / skills / packages / image, not fall back to the host's
        // COPPERCLAW_DEFAULT_MODEL. Surfaced live as a parent on a capable
        // model spawning six builders that all booted on the weak
        // host-default local model and produced nothing.
        let (handler, central, _tmp, parent, target) = make_handler(always_allow());

        // Seed the parent group with a distinctive container config.
        container_configs::upsert(
            &central,
            UpsertContainerConfig {
                agent_group_id: parent.agent_group_id,
                provider: Some("anthropic".into()),
                model: Some("qwen/qwen3.7-max".into()),
                effort: Some(copperclaw_types::Effort::High),
                image_tag: Some("copperclaw/session:test-tag".into()),
                assistant_name: Some("ParentName".into()),
                max_messages_per_prompt: Some(7),
                skills: container_configs::SkillsSelector::All,
                mcp_servers: serde_json::json!({}),
                packages_apt: vec!["jq".into()],
                packages_npm: vec!["typescript".into()],
                additional_mounts: serde_json::json!([]),
                cli_scope: container_configs::CliScope::Group,
                config_fingerprint: Some("deadbeef".into()),
                egress_allow: vec![],
                resource_limits: serde_json::json!({}),
                coding_enabled: true,
                surface_thinking: false,
                tool_profile: None,
            },
        )
        .unwrap();

        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "builder-bot",
                    "instructions": "Implement the feature under /workspace.",
                }),
                target,
                session_id: None,
                row_id: None,
            })
            .unwrap();

        let child = agent_groups::list(&central)
            .unwrap()
            .into_iter()
            .find(|g| g.name == "builder-bot")
            .expect("child group present");
        let child_cfg = container_configs::get(&central, child.id)
            .unwrap()
            .expect("child inherited a container config row");

        assert_eq!(child_cfg.model.as_deref(), Some("qwen/qwen3.7-max"));
        assert_eq!(child_cfg.provider.as_deref(), Some("anthropic"));
        assert_eq!(child_cfg.effort, Some(copperclaw_types::Effort::High));
        assert_eq!(
            child_cfg.image_tag.as_deref(),
            Some("copperclaw/session:test-tag"),
        );
        // image_tag + matching fingerprint => child adopts the parent's
        // image with no rebuild.
        assert_eq!(child_cfg.config_fingerprint.as_deref(), Some("deadbeef"));
        assert_eq!(child_cfg.packages_apt, vec!["jq".to_string()]);
        assert_eq!(child_cfg.packages_npm, vec!["typescript".to_string()]);
        assert!(child_cfg.coding_enabled);
        // The parent's identity name is deliberately NOT inherited.
        assert_eq!(child_cfg.assistant_name, None);
    }

    #[test]
    fn child_without_parent_config_uses_host_defaults() {
        // When the parent has no container_configs row it runs on host
        // defaults; the child must too (no row, no panic) — the inheritance
        // copy is a no-op rather than writing an empty config.
        let (handler, central, _tmp, _parent, target) = make_handler(always_allow());

        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "builder-bot",
                    "instructions": "Implement the feature under /workspace.",
                }),
                target,
                session_id: None,
                row_id: None,
            })
            .unwrap();

        let child = agent_groups::list(&central)
            .unwrap()
            .into_iter()
            .find(|g| g.name == "builder-bot")
            .expect("child group present");
        assert!(
            container_configs::get(&central, child.id)
                .unwrap()
                .is_none(),
            "no parent config => no child config row (falls back to host defaults)",
        );
    }

    #[test]
    fn child_inbound_is_seeded_with_instructions_so_container_spawns() {
        // Regression for the "spawned 3 research agents and none ever
        // reported back" failure mode. Before this fix the child agent
        // existed in the DB but had no pending inbound — the container
        // manager spawns sessions only when there's queued work, so
        // children sat inert forever. Now create_agent writes the
        // operator's `instructions` as the child's first chat message
        // with `trigger=true` so the manager picks it up on the next
        // reconcile.
        let (handler, central, tmp, _parent, target) = make_handler(always_allow());
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "research-bot",
                    "instructions": "Search for the latest AI safety papers and summarise."
                }),
                target,
                session_id: None,
                row_id: None,
            })
            .unwrap();
        // Locate the newly-created child and read its inbound.db.
        let child_session = sessions::list_active(&central)
            .unwrap()
            .into_iter()
            .find(|s| {
                agent_groups::get(&central, s.agent_group_id)
                    .map(|g| g.name == "research-bot")
                    .unwrap_or(false)
            })
            .expect("child session exists");
        let paths = SessionPaths::new(tmp.path(), child_session.agent_group_id, child_session.id);
        let conn = copperclaw_db::session::open_inbound(&paths).unwrap();
        let pending = messages_in_read::get_pending(&conn, true, 10).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "child must have exactly one seeded inbound (the instructions)"
        );
        let row = &pending[0];
        assert_eq!(row.kind, MessageKind::Chat, "kicker is a chat-kind message");
        let text = row
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .expect("content.text present");
        assert!(
            text.contains("research-bot") && text.contains("AI safety papers"),
            "kicker must name the agent and quote the instructions, got: {text}"
        );
    }

    #[test]
    fn child_inherits_parent_session_routing_so_outbound_can_deliver() {
        // Regression: child sessions had no `session_routing` record,
        // so even with the parent's messaging_group_id inherited, the
        // delivery service marked every outbound `chat` row failed with
        // NoRoute. The router writes session_routing on inbound arrival,
        // but spawned sessions have no inbound event — create_agent now
        // copies the parent's routing record into the child's inbound.db.
        use copperclaw_db::tables::session_routing;
        let (handler, central, tmp, parent, target) = make_handler(always_allow());
        // Seed parent's session_routing — the production code path
        // writes this on inbound arrival; the test fixture doesn't go
        // through that path, so we write it directly.
        let parent_paths = SessionPaths::new(tmp.path(), parent.agent_group_id, parent.session_id);
        let parent_conn = copperclaw_db::session::open_inbound(&parent_paths).unwrap();
        let parent_routing = copperclaw_types::routing::SessionRouting {
            channel_type: Some(ChannelType::new("telegram")),
            platform_id: Some("bot-123".into()),
            thread_id: Some("chat-456".into()),
        };
        session_routing::write(&parent_conn, &parent_routing).unwrap();
        // Spawn the child.
        let before_sessions: std::collections::HashSet<SessionId> = sessions::list_active(&central)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "scout",
                    "instructions": "go look",
                }),
                target,
                session_id: None,
                row_id: None,
            })
            .unwrap();
        let child_session = sessions::list_active(&central)
            .unwrap()
            .into_iter()
            .find(|s| !before_sessions.contains(&s.id))
            .expect("child created");
        let child_paths =
            SessionPaths::new(tmp.path(), child_session.agent_group_id, child_session.id);
        let child_conn = copperclaw_db::session::open_inbound(&child_paths).unwrap();
        let child_routing = session_routing::read(&child_conn)
            .unwrap()
            .expect("child must have routing copied from parent");
        assert_eq!(child_routing.channel_type, parent_routing.channel_type);
        assert_eq!(child_routing.platform_id, parent_routing.platform_id);
        assert_eq!(child_routing.thread_id, parent_routing.thread_id);
    }

    #[test]
    fn child_session_inherits_parent_messaging_group() {
        // Regression for the live Telegram failure: child sessions used
        // to land with `messaging_group_id = NULL`, so `send_message`
        // from the child had no return path. Now create_agent copies
        // the parent session's routing onto the new session.
        let (handler, central, _tmp, parent, target) = make_handler(always_allow());
        let before_sessions: std::collections::HashSet<SessionId> = sessions::list_active(&central)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "scout",
                    "instructions": "go look",
                }),
                target,
                session_id: None,
                row_id: None,
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
    fn child_session_records_source_session_id_pointing_at_parent() {
        // Phase 1 of docs/plans/agent-to-agent-routing.md: child
        // sessions must persist a back-reference to the spawning
        // session so the runtime can route default `send_message`
        // calls up to the parent instead of the user's chat.
        let (handler, central, _tmp, parent, target) = make_handler(always_allow());
        let before_sessions: std::collections::HashSet<SessionId> = sessions::list_active(&central)
            .unwrap()
            .iter()
            .map(|s| s.id)
            .collect();
        handler
            .handle(DeliveryActionInput {
                action: "create_agent".into(),
                payload: serde_json::json!({
                    "name": "scout",
                    "instructions": "go look",
                }),
                target,
                session_id: None,
                row_id: None,
            })
            .unwrap();
        let new_session = sessions::list_active(&central)
            .unwrap()
            .into_iter()
            .find(|s| !before_sessions.contains(&s.id))
            .expect("a new session was created");
        assert_eq!(
            new_session.source_session_id,
            Some(parent.session_id),
            "child must record the spawning parent's session id so default \
             `send_message` routes back up to the parent's inbound, not the \
             user's chat",
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
                row_id: None,
            })
            .unwrap();
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1, "exactly one result row landed in parent");
        let r = &results[0]["create_agent_result"];
        assert_eq!(r["status"], "created");
        assert!(r["session_id"].is_string(), "real session id present");
        assert!(
            r["agent_group_id"].is_string(),
            "real agent group id present"
        );
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
                row_id: None,
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
                row_id: None,
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
        agent_groups::set_subagent_depth(
            &central,
            parent.agent_group_id,
            DEFAULT_MAX_SUBAGENT_DEPTH,
        )
        .unwrap();
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
                row_id: None,
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
        agent_groups::set_subagent_depth(&central, parent.agent_group_id, 2).unwrap();
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
                row_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before + 1, "depth-3 spawn must create the group");
        // The new group should be persisted at depth 3 so a further
        // spawn from it would be the one that fails. We read straight
        // from the DB now that there's no in-memory cache.
        let new_child = agent_groups::list(&central)
            .unwrap()
            .into_iter()
            .find(|g| g.name == "depth-three-child")
            .expect("child row present");
        let depth = agent_groups::get_subagent_depth(&central, new_child.id)
            .unwrap()
            .expect("depth persisted");
        assert_eq!(depth, 3);
    }

    #[test]
    fn create_agent_with_max_depth_one_reproduces_historical_behaviour() {
        // Pin `max_depth = 1` on the handler. Then a parent recorded at
        // depth 1 spawning would land the child at depth 2 — rejected.
        let (mut handler, central, tmp, parent, target) = make_handler(always_allow());
        handler.deps.max_depth = 1;
        agent_groups::set_subagent_depth(&central, parent.agent_group_id, 1).unwrap();
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
                row_id: None,
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
                row_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(after, before, "invalid payload must not create rows");
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "invalid");
    }

    // -----------------------------------------------------------------------
    // Code-review fixes — depth-cap TOCTOU, restart persistence,
    // saturation, poison handling, orphan-warn.
    // -----------------------------------------------------------------------

    /// Finding 4 (TOCTOU): cap is re-checked under `depth_gate`. The
    /// in-memory cache that used to back the re-check was removed (it
    /// grew without bound and gave wrong answers when an
    /// `AgentGroupId` was reused after deletion), so the lock is now
    /// just a process-wide mutex and the re-read goes straight to the
    /// DB. We can't actually race threads in a unit test, so we model
    /// the loser by writing the winner's commit directly to the DB
    /// before invoking the handler; the re-check under the gate must
    /// observe the bumped value and reject.
    #[test]
    fn create_agent_depth_recheck_under_lock_catches_concurrent_winner() {
        let (mut handler, central, tmp, parent, target) = make_handler(always_allow());
        handler.deps.max_depth = 2;
        // Persist the winner's commit: parent is now at depth 2 in
        // the DB. Both the soft check (step 3) and the under-lock
        // re-check (step 4) will read 2 + 1 = 3 > max_depth (2),
        // which is what would happen to the loser in a real race
        // where the winner finished its central-DB write between the
        // loser's two read points.
        agent_groups::set_subagent_depth(&central, parent.agent_group_id, 2).unwrap();

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
                row_id: None,
            })
            .unwrap();
        let after = agent_groups::list(&central).unwrap().len();
        assert_eq!(
            after, before,
            "racing loser must not create rows once DB shows the winner"
        );
        let results = read_inbound_create_results(tmp.path(), parent);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["create_agent_result"]["status"], "rejected");
    }

    /// Finding 5 (persistence): depth cap survives module reconstruction.
    #[test]
    fn depth_cap_survives_module_reconstruction() {
        let (handler, central, tmp, parent, target) = make_handler(always_allow());
        // Persist parent at the cap directly via the DB.
        agent_groups::set_subagent_depth(
            &central,
            parent.agent_group_id,
            DEFAULT_MAX_SUBAGENT_DEPTH,
        )
        .unwrap();

        // "Restart": new module. There is no in-memory depth cache
        // any more; the gate reads the DB on every call so this
        // assertion is now structural — the freshly-built handler
        // exposes nothing per-agent to inspect, by design.
        let module =
            CreateAgentModule::new(central.clone(), tmp.path().to_path_buf(), always_allow());
        let fresh = CreateAgentHandler {
            deps: module.deps().clone(),
        };
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
                row_id: None,
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
        agent_groups::set_subagent_depth(
            &central,
            parent.agent_group_id,
            MAX_SUBAGENT_DEPTH_CEILING,
        )
        .unwrap();
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
                row_id: None,
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
        agent_groups::set_subagent_depth(&central, parent.agent_group_id, u8::MAX).unwrap();
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
                row_id: None,
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
                    row_id: None,
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

    // -----------------------------------------------------------------------
    // Bounded-memory: the handler must not retain per-AgentGroupId state.
    // -----------------------------------------------------------------------

    /// Push 10 000 distinct `AgentGroupId`s through the depth lookup
    /// and assert the handler grows nothing per-id. Before this audit
    /// the handler kept an `Arc<Mutex<HashMap<AgentGroupId, u8>>>`
    /// cache that grew once per ever-spawned group; on a long-running
    /// host with many short-lived groups that was a slow memory leak
    /// and a correctness bug (stale entries for deleted-then-reused
    /// ids). The fix is to read depth straight from the DB on every
    /// call — this test pins that contract by exercising the lookup
    /// at volume and re-asserting it on the persisted column rather
    /// than any in-process map.
    #[test]
    fn lookup_parent_depth_does_not_grow_per_agent_group() {
        // Seed 10k distinct agent groups with a known depth each,
        // then ask the handler to look each one up. If a cache
        // existed it would be 10k entries large by the end; with the
        // DB-backed lookup the only per-call state is the SELECT
        // result which dies at the end of each iteration.
        const N: usize = 10_000;
        let (handler, central, _tmp, _parent, _target) = make_handler(always_allow());
        let mut ids: Vec<AgentGroupId> = Vec::with_capacity(N);
        for i in 0..N {
            let ag = agent_groups::create(
                &central,
                CreateAgentGroup {
                    name: format!("bulk-{i}"),
                    folder: format!("bulk-{i}"),
                    agent_provider: None,
                },
            )
            .unwrap();
            // Persist a depth so the lookup returns Some, exercising
            // the same DB path the real gate hits.
            agent_groups::set_subagent_depth(&central, ag.id, 2).unwrap();
            ids.push(ag.id);
        }
        for &id in &ids {
            let parent = ParentSession {
                session_id: SessionId::new(),
                agent_group_id: id,
            };
            let d = handler.lookup_parent_depth(Some(&parent));
            assert_eq!(d, Some(2));
        }
        // Structural check: the handler's only piece of in-process
        // synchronisation state is the `()`-payload `depth_gate`
        // mutex. There is no `HashMap<AgentGroupId, _>` to bloat;
        // this binding documents the invariant in the type system —
        // if someone re-introduces a per-id cache the type here
        // changes and this line breaks.
        let _: &Arc<Mutex<()>> = &handler.deps.depth_gate;
    }

    /// A re-used `AgentGroupId` (e.g. an admin tool reset the row's
    /// `subagent_depth`, or a future schema permits the id to be
    /// recycled) must NOT see stale depth from a prior reading. The
    /// old cache returned the previous value forever; the DB-backed
    /// lookup correctly returns the freshly persisted one.
    #[test]
    fn lookup_parent_depth_does_not_return_stale_on_depth_reset() {
        let (handler, central, _tmp, _parent, _target) = make_handler(always_allow());
        let ag = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "recycled".into(),
                folder: "recycled".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let id = ag.id;
        // First reading: depth 3.
        agent_groups::set_subagent_depth(&central, id, 3).unwrap();
        let parent = ParentSession {
            session_id: SessionId::new(),
            agent_group_id: id,
        };
        assert_eq!(handler.lookup_parent_depth(Some(&parent)), Some(3));
        // Reset to depth 1 in the DB. The handler must observe the
        // new value, not a cached 3.
        agent_groups::set_subagent_depth(&central, id, 1).unwrap();
        assert_eq!(handler.lookup_parent_depth(Some(&parent)), Some(1));
    }
}
