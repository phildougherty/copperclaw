//! Per-resource CRUD modules.
//!
//! Each module owns one table on the central DB or the per-session DBs.
//! See `PLAN.md` § A1 for the function inventory each module must export.
//!
//! Most table modules are stubs to be implemented by parallel teams; see
//! [`agent_groups`] for the exemplar pattern.

pub mod agent_group_members;
pub mod agent_groups;
pub mod audit_log;
pub mod messages_in;
pub mod messages_out;
pub mod messaging_group_agents;
pub mod messaging_groups;
pub mod sessions;
pub mod user_dms;
pub mod user_roles;
pub mod users;
pub mod pending_questions;
pub mod pending_approvals;
pub mod pending_sender_approvals;
pub mod pending_channel_approvals;
pub mod delivered;
pub mod destinations;
pub mod session_routing;
pub mod processing_ack;
pub mod session_state;
pub mod container_state;
pub mod agent_destinations;
pub mod unregistered_senders;
pub mod dropped_messages;
pub mod container_configs;
