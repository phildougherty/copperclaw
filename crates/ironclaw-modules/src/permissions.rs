//! Role-based access control.
//!
//! Defines the [`Role`] hierarchy used by the host's access gate, plus a
//! pure [`check`] function that maps a `(role, op)` pair to a yes/no.
//!
//! The host's `ironclaw-db::user_roles` table currently uses a different
//! role enum (`Owner`/`Admin`); that table is used for global ACL on the
//! central DB and is intentionally distinct from the module-side role
//! hierarchy here. The plan (T7) calls for `Admin`/`Member`/`Guest` at the
//! module surface.

use crate::context::{
    GateCtx, GateDecision, Module, ModuleContext, SenderScopeCtx, SenderScopeDecision,
};
use crate::error::ModuleError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Role hierarchy, ordered most-privileged to least.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Member,
    Guest,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Member => "member",
            Self::Guest => "guest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            "guest" => Some(Self::Guest),
            _ => None,
        }
    }

    /// Numeric rank — higher is more privileged.
    pub fn rank(self) -> u8 {
        match self {
            Self::Admin => 3,
            Self::Member => 2,
            Self::Guest => 1,
        }
    }
}

/// Discrete operations that can be permission-gated.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOp {
    /// Create a new agent group.
    CreateAgent,
    /// Modify an existing agent group's wiring (engage rule, sender scope).
    EditWiring,
    /// Approve a previously-blocked sender.
    ApproveSender,
    /// Approve a previously-blocked channel.
    ApproveChannel,
    /// Approve an `install_packages` self-mod request.
    InstallPackages,
    /// Approve an `add_mcp_server` self-mod request.
    AddMcpServer,
    /// Schedule a task (cron or one-shot).
    ScheduleTask,
    /// Cancel any other user's scheduled task.
    CancelOtherTask,
    /// Send a message via the agent on this messaging group.
    SendMessage,
}

impl PermissionOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreateAgent => "create_agent",
            Self::EditWiring => "edit_wiring",
            Self::ApproveSender => "approve_sender",
            Self::ApproveChannel => "approve_channel",
            Self::InstallPackages => "install_packages",
            Self::AddMcpServer => "add_mcp_server",
            Self::ScheduleTask => "schedule_task",
            Self::CancelOtherTask => "cancel_other_task",
            Self::SendMessage => "send_message",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "create_agent" => Some(Self::CreateAgent),
            "edit_wiring" => Some(Self::EditWiring),
            "approve_sender" => Some(Self::ApproveSender),
            "approve_channel" => Some(Self::ApproveChannel),
            "install_packages" => Some(Self::InstallPackages),
            "add_mcp_server" => Some(Self::AddMcpServer),
            "schedule_task" => Some(Self::ScheduleTask),
            "cancel_other_task" => Some(Self::CancelOtherTask),
            "send_message" => Some(Self::SendMessage),
            _ => None,
        }
    }
}

/// Returns `true` if `role` may perform `op` under the default policy.
pub fn check(role: Role, op: PermissionOp) -> bool {
    match op {
        // Admin-only.
        PermissionOp::CreateAgent
        | PermissionOp::EditWiring
        | PermissionOp::ApproveSender
        | PermissionOp::ApproveChannel
        | PermissionOp::InstallPackages
        | PermissionOp::AddMcpServer
        | PermissionOp::CancelOtherTask => role == Role::Admin,
        // Members + Admins.
        PermissionOp::ScheduleTask => role.rank() >= Role::Member.rank(),
        // Everybody (including Guest) can send a message.
        PermissionOp::SendMessage => true,
    }
}

/// Closure that resolves a `GateCtx` into a `Role` (or `None` if the user is
/// not in the host's role table).
pub type RoleLookup = Arc<dyn Fn(&GateCtx) -> Option<Role> + Send + Sync>;

/// Module impl. Holds a directory mapping `UserId` -> `Role` so the access
/// gate can map a `GateCtx.user` into a role for the [`check`] function.
pub struct PermissionsModule {
    /// Best-effort lookup table. In production the host injects this from
    /// the `user_roles` table; in tests we inline it.
    role_lookup: RoleLookup,
    /// Default role assigned to unknown users.
    default_role: Role,
}

impl PermissionsModule {
    /// Build a module from a lookup function.
    pub fn new<F>(lookup: F, default_role: Role) -> Self
    where
        F: Fn(&GateCtx) -> Option<Role> + Send + Sync + 'static,
    {
        Self {
            role_lookup: Arc::new(lookup),
            default_role,
        }
    }

    /// Convenience constructor that uses a static `HashMap` of users to
    /// roles plus a default for unknown users.
    pub fn from_table(
        table: HashMap<ironclaw_types::UserId, Role>,
        default_role: Role,
    ) -> Self {
        Self::new(
            move |ctx: &GateCtx| ctx.user.and_then(|u| table.get(&u).copied()),
            default_role,
        )
    }

    /// Default-deny module: every user is treated as `Guest` and the lookup
    /// returns `None`.
    pub fn deny_all() -> Self {
        Self::new(|_ctx| None, Role::Guest)
    }

    pub fn default_role(&self) -> Role {
        self.default_role
    }

    /// Resolve the role for `ctx`, falling back to the default role.
    pub fn role_for(&self, ctx: &GateCtx) -> Role {
        (self.role_lookup)(ctx).unwrap_or(self.default_role)
    }
}

#[async_trait]
impl Module for PermissionsModule {
    fn name(&self) -> &'static str {
        "permissions"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        let lookup = Arc::clone(&self.role_lookup);
        let default_role = self.default_role;
        let access_lookup = Arc::clone(&lookup);
        ctx.set_access_gate(Arc::new(move |g: GateCtx| {
            let role = (access_lookup)(&g).unwrap_or(default_role);
            let Some(op) = PermissionOp::parse(&g.op) else {
                return GateDecision::Defer;
            };
            if check(role, op) {
                GateDecision::Allow
            } else {
                GateDecision::Deny(format!(
                    "role `{}` cannot perform `{}`",
                    role.as_str(),
                    op.as_str()
                ))
            }
        }));
        let scope_lookup = Arc::clone(&lookup);
        ctx.set_sender_scope_gate(Arc::new(move |s: SenderScopeCtx| {
            // A resolved user with any known role passes the scope gate.
            let synthetic = GateCtx {
                user: s.resolved_user,
                agent_group_id: Some(s.agent_group_id),
                messaging_group_id: s.messaging_group_id,
                op: "sender_scope".to_owned(),
            };
            match (scope_lookup)(&synthetic) {
                Some(_role) => SenderScopeDecision::Allow,
                None => SenderScopeDecision::Defer,
            }
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockModuleContext;
    use ironclaw_types::{AgentGroupId, UserId};

    #[test]
    fn role_as_str_and_parse_roundtrip() {
        for r in [Role::Admin, Role::Member, Role::Guest] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert!(Role::parse("nope").is_none());
    }

    #[test]
    fn role_rank_orders_admin_member_guest() {
        assert!(Role::Admin.rank() > Role::Member.rank());
        assert!(Role::Member.rank() > Role::Guest.rank());
    }

    #[test]
    fn role_serde_roundtrip() {
        for r in [Role::Admin, Role::Member, Role::Guest] {
            let json = serde_json::to_string(&r).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn op_as_str_and_parse_roundtrip() {
        for op in [
            PermissionOp::CreateAgent,
            PermissionOp::EditWiring,
            PermissionOp::ApproveSender,
            PermissionOp::ApproveChannel,
            PermissionOp::InstallPackages,
            PermissionOp::AddMcpServer,
            PermissionOp::ScheduleTask,
            PermissionOp::CancelOtherTask,
            PermissionOp::SendMessage,
        ] {
            assert_eq!(PermissionOp::parse(op.as_str()), Some(op));
        }
        assert!(PermissionOp::parse("nope").is_none());
    }

    #[test]
    fn admin_can_do_everything() {
        for op in [
            PermissionOp::CreateAgent,
            PermissionOp::EditWiring,
            PermissionOp::ApproveSender,
            PermissionOp::ApproveChannel,
            PermissionOp::InstallPackages,
            PermissionOp::AddMcpServer,
            PermissionOp::ScheduleTask,
            PermissionOp::CancelOtherTask,
            PermissionOp::SendMessage,
        ] {
            assert!(check(Role::Admin, op), "admin should be allowed for {op:?}");
        }
    }

    #[test]
    fn member_can_send_and_schedule_only() {
        assert!(check(Role::Member, PermissionOp::SendMessage));
        assert!(check(Role::Member, PermissionOp::ScheduleTask));
        for op in [
            PermissionOp::CreateAgent,
            PermissionOp::EditWiring,
            PermissionOp::ApproveSender,
            PermissionOp::ApproveChannel,
            PermissionOp::InstallPackages,
            PermissionOp::AddMcpServer,
            PermissionOp::CancelOtherTask,
        ] {
            assert!(!check(Role::Member, op), "member should NOT be allowed for {op:?}");
        }
    }

    #[test]
    fn guest_can_send_only() {
        assert!(check(Role::Guest, PermissionOp::SendMessage));
        for op in [
            PermissionOp::CreateAgent,
            PermissionOp::EditWiring,
            PermissionOp::ApproveSender,
            PermissionOp::ApproveChannel,
            PermissionOp::InstallPackages,
            PermissionOp::AddMcpServer,
            PermissionOp::ScheduleTask,
            PermissionOp::CancelOtherTask,
        ] {
            assert!(!check(Role::Guest, op), "guest should NOT be allowed for {op:?}");
        }
    }

    #[test]
    fn from_table_lookup_works() {
        let u = UserId::new();
        let mut table = HashMap::new();
        table.insert(u, Role::Admin);
        let m = PermissionsModule::from_table(table, Role::Guest);
        let ctx = GateCtx {
            user: Some(u),
            agent_group_id: None,
            messaging_group_id: None,
            op: "create_agent".into(),
        };
        assert_eq!(m.role_for(&ctx), Role::Admin);
        let ctx_unknown = GateCtx {
            user: Some(UserId::new()),
            agent_group_id: None,
            messaging_group_id: None,
            op: "create_agent".into(),
        };
        assert_eq!(m.role_for(&ctx_unknown), Role::Guest);
    }

    #[test]
    fn deny_all_defaults_to_guest() {
        let m = PermissionsModule::deny_all();
        let ctx = GateCtx {
            user: Some(UserId::new()),
            agent_group_id: None,
            messaging_group_id: None,
            op: "create_agent".into(),
        };
        assert_eq!(m.role_for(&ctx), Role::Guest);
        assert_eq!(m.default_role(), Role::Guest);
    }

    #[tokio::test]
    async fn install_registers_both_gates() {
        let m = PermissionsModule::deny_all();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let regs = ctx.registered();
        assert!(regs.contains(&"access_gate"));
        assert!(regs.contains(&"sender_scope_gate"));
    }

    #[tokio::test]
    async fn access_gate_allow_for_admin_create_agent() {
        let u = UserId::new();
        let mut table = HashMap::new();
        table.insert(u, Role::Admin);
        let m = PermissionsModule::from_table(table, Role::Guest);
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.access_gates.lock().unwrap()[0].clone();
        let decision = (gate)(GateCtx {
            user: Some(u),
            agent_group_id: Some(AgentGroupId::new()),
            messaging_group_id: None,
            op: "create_agent".into(),
        });
        assert!(decision.is_allow());
    }

    #[tokio::test]
    async fn access_gate_deny_for_guest_create_agent() {
        let m = PermissionsModule::deny_all();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.access_gates.lock().unwrap()[0].clone();
        let decision = (gate)(GateCtx {
            user: Some(UserId::new()),
            agent_group_id: None,
            messaging_group_id: None,
            op: "create_agent".into(),
        });
        assert!(decision.is_deny());
        if let GateDecision::Deny(reason) = decision {
            assert!(reason.contains("guest"));
            assert!(reason.contains("create_agent"));
        }
    }

    #[tokio::test]
    async fn access_gate_defers_on_unknown_op() {
        let m = PermissionsModule::deny_all();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.access_gates.lock().unwrap()[0].clone();
        let decision = (gate)(GateCtx {
            user: Some(UserId::new()),
            agent_group_id: None,
            messaging_group_id: None,
            op: "made_up_op".into(),
        });
        assert!(decision.is_defer());
    }

    #[tokio::test]
    async fn sender_scope_allow_for_known_user() {
        let u = UserId::new();
        let mut table = HashMap::new();
        table.insert(u, Role::Member);
        let m = PermissionsModule::from_table(table, Role::Guest);
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: None,
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: Some(u),
        });
        assert!(decision.is_allow());
    }

    #[tokio::test]
    async fn sender_scope_defers_for_unknown_user() {
        let m = PermissionsModule::deny_all();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: None,
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        assert!(decision.is_defer());
    }
}
