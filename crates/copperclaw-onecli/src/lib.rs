//! `OneCLI` Agent Vault HTTP client — credential injection for spawned containers.
//!
//! See `PLAN.md` § 6 (T11).
//!
//! This crate provides the small typed HTTP surface copperclaw uses to talk to
//! the Agent Vault service that owns secrets and approval state. It is the
//! only path through which the host injects vault-managed credentials into a
//! spawned container: the vault holds the secret material, copperclaw merely
//! tells it which agent slug it is provisioning for and what environment /
//! mount layout the container expects, then asks the vault to approve or deny
//! sensitive actions raised by the running agent.
//!
//! The wire shapes live in [`types`], the failure modes in [`error`], and the
//! transport in [`client`]. Re-exports at the crate root keep call sites
//! short.

#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod types;

pub use client::OneCliClient;
pub use error::OneCliError;
pub use types::{
    AgentSummary, ContainerProvisioning, EnvVarSpec, NetworkPolicy, PendingApproval,
    SecretMountSpec,
};
