//! Pluggable host-side modules.
//!
//! Modules are behaviors that hook into the host's router and delivery loops
//! via the [`ModuleContext`] trait. Each module is a struct implementing
//! [`Module`]; the host's boot sequence calls [`Module::install`] on every
//! registered module in priority order, and each one wires the hooks it cares
//! about. The host (T3) implements [`ModuleContext`] against its internal hook
//! chain.
//!
//! Modules in this crate:
//!
//! * [`typing`] — emits typing indicators to channel adapters while the
//!   container is processing a turn.
//! * [`mount_security`] — validates bind-mount paths for symlink / traversal
//!   attacks.
//! * [`permissions`] — role-based access control (Admin / Member / Guest).
//! * [`approvals`] — pending sender / channel approval flow.
//! * [`interactive`] — backs the `ask_user_question` and `send_card` tools.
//! * [`scheduling`] — cron + one-shot scheduling engine.
//! * [`agent_to_agent`] — resolves `to: agent:<name>` destinations.
//! * [`self_mod`] — backs the `install_packages` and `add_mcp_server` tools.

pub mod agent_to_agent;
pub mod approvals;
pub mod context;
pub mod error;
pub mod interactive;
pub mod mount_security;
pub mod permissions;
pub mod scheduling;
pub mod self_mod;
pub mod typing;

pub use agent_to_agent::{
    AgentDispatchModule, AgentRef, AgentToAgentModule, CreateAgentHandler, CreateAgentModule,
    CreateAgentPermissionCheck, CreateAgentPermissionCtx,
    always_allow as create_agent_always_allow, always_deny as create_agent_always_deny,
    users_table_check as create_agent_users_table_check,
};
pub use approvals::{ApprovalSummary, ApprovalsModule, NewPendingCtx, NewPendingNotifier};
pub use context::{
    ChannelRequestCtx, DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput,
    DeliveryDispatcher, DispatchTarget, GateCtx, GateDecision, InterceptorCtx, InterceptorDecision,
    Module, ModuleContext, MountHostContext, SenderResolver, SenderScopeCtx, SenderScopeDecision,
};
pub use error::ModuleError;
pub use interactive::{InteractiveModule, PendingQuestion, QuestionId};
pub use mount_security::{MountError, MountSecurityModule, validate_mount_target};
pub use permissions::{PermissionOp, PermissionsModule, Role, check as permissions_check};
pub use scheduling::{
    CreateTaskSpec, InMemoryTaskStore, ScheduleError, ScheduleHandler, SchedulingModule,
    TaskRecord, TaskStatus as ScheduledTaskStatus, TaskStore, UpdateTaskFields, When,
    compute_next_fire, parse_when,
};
pub use self_mod::{ChangeRequest, PackageError, PackageManager, SelfModModule};
pub use typing::{TypingConfig, TypingModule};
