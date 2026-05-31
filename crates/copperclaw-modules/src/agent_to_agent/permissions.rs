//! Permission gating for the `create_agent` delivery action.
//!
//! The handler consults a [`CreateAgentPermissionCheck`] closure before
//! it mutates the central DB. Production wiring uses
//! [`users_table_check`] which inspects `user_roles`; tests use the
//! [`always_allow`] / [`always_deny`] convenience factories.

use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::user_roles;
use copperclaw_types::{AgentGroupId, SessionId};
use std::sync::Arc;

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
/// * Operator runs `cclaw users grant <id> admin` or `owner` → allow.
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

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_groups::{self, CreateAgentGroup};

    /// `users_table_check` denies when the install has no roles
    /// granted. This is the bootstrap-safe default — without it any
    /// untrusted operator could spawn agents the moment the host
    /// boots.
    #[test]
    fn users_table_check_denies_on_empty_install() {
        use copperclaw_db::tables::users::{self, UpsertUser};
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
        use copperclaw_db::tables::users::{self, UpsertUser};
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
        use copperclaw_db::tables::users::{self, UpsertUser};
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
}
