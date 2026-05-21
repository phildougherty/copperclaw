//! Inbound router. Resolves messaging groups, fans out to agents, writes
//! `messages_in` rows. See `PLAN.md` § 6 T3.
//!
//! The crate exposes one principal type, [`Router`], plus the supporting
//! [`HookChain`], [`SessionRoot`], and outcome enums. Modules wire host-side
//! behavior through [`HookChain`]; the host's `ModuleContext` adapter routes
//! [`ironclaw_modules::context::ModuleContext`] setters at this chain. See
//! the crate-level tests for end-to-end examples.

#![doc(html_root_url = "https://docs.rs/ironclaw-host-router/0.1.0")]

pub mod debounce;
pub mod error;
pub mod hooks;
pub mod route;
pub mod session;

pub use debounce::{DebounceKey, Debouncer, InflightKey, InflightSet, DEBOUNCE_WINDOW};
pub use error::RouterError;
pub use hooks::HookChain;
pub use route::{DeliveredTo, DropReason, PendingReason, RouteOutcome, Router};
pub use session::{FsSessionRoot, SessionPool, SessionRoot};

#[cfg(test)]
mod smoke {
    //! Public-surface smoke tests — ensure every `pub use` re-export resolves
    //! at the crate root and that the high-level types are constructible from
    //! outside the inner modules.

    use super::*;
    use ironclaw_db::central::CentralDb;
    use ironclaw_types::{AgentGroupId, MessageId, SessionId};
    use std::sync::Arc;

    #[test]
    fn re_exports_resolve() {
        let _: Option<RouterError> = None;
        let _ = DEBOUNCE_WINDOW;
        let _ = HookChain::new();
        let _ = Debouncer::new();
        let _ = InflightSet::new();
        let _ = DropReason::NoAgents;
        let _ = PendingReason::SenderUnregistered;
        let _ = DeliveredTo {
            agent_group_id: AgentGroupId::new(),
            session_id: SessionId::new(),
            message_id: MessageId::new(),
            seq: 2,
        };
    }

    #[test]
    fn router_constructible_at_crate_root() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
        let _ = Router::new(db, root);
    }
}
