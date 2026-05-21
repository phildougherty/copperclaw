//! Wire types for the `OneCLI` HTTP API.
//!
//! The Agent Vault service is owned by another team; the shapes here define
//! the JSON contract that this client expects. They intentionally mirror only
//! the subset of fields ironclaw needs and stay independent of any database
//! row representation.

use serde::{Deserialize, Serialize};

/// Compact representation of an agent record returned by the vault.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    /// Stable, server-assigned identifier (opaque string).
    pub id: String,
    /// Caller-supplied slug. Unique within the vault tenant.
    pub slug: String,
    /// Optional human-friendly label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// A single environment variable to inject into the spawned container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVarSpec {
    /// Environment variable name (e.g. `OPENAI_API_KEY`).
    pub name: String,
    /// Vault-side secret reference resolved at container start time.
    pub secret_ref: String,
}

/// A secret file to be mounted inside the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretMountSpec {
    /// Absolute path inside the container where the secret should appear.
    pub mount_path: String,
    /// Vault-side secret reference resolved at container start time.
    pub secret_ref: String,
    /// File mode in octal (e.g. `0o400`). Defaults to read-only owner when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// Coarse-grained outbound network policy applied by the vault.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy {
    /// No outbound network access permitted.
    #[default]
    Deny,
    /// Only the explicit allow-list returned by the vault is permitted.
    Allowlist,
    /// Unrestricted outbound access.
    Open,
}

/// Provisioning payload that ironclaw pushes to `OneCLI` to describe how a
/// container's credentials should be injected.
///
/// This is a deliberate subset of the host-side `container_configs` row: only
/// fields the vault needs (env vars, secret mounts, network policy) appear
/// here. CLI scope, packages, MCP servers etc. live elsewhere and are not
/// part of the vault contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ContainerProvisioning {
    /// Environment-variable secrets to inject.
    #[serde(default)]
    pub env: Vec<EnvVarSpec>,
    /// File-mounted secrets to inject.
    #[serde(default)]
    pub mounts: Vec<SecretMountSpec>,
    /// Network policy enforced for outbound traffic from the container.
    #[serde(default = "default_network_policy")]
    pub network_policy: NetworkPolicy,
    /// When `network_policy` is `Allowlist`, the egress hosts/CIDRs permitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_egress: Vec<String>,
}

fn default_network_policy() -> NetworkPolicy {
    NetworkPolicy::Deny
}

/// A pending approval request emitted by the vault when an agent attempts a
/// sensitive action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// Server-assigned approval identifier.
    pub id: String,
    /// Owning agent's vault identifier.
    pub agent_id: String,
    /// Short machine-readable action label (e.g. `read_secret`).
    pub action: String,
    /// Optional free-form context shown to the operator deciding the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// RFC 3339 timestamp at which the request was raised.
    pub requested_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_summary_roundtrip_full() {
        let a = AgentSummary {
            id: "ag_123".into(),
            slug: "team-greeter".into(),
            display_name: Some("Greeter".into()),
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(
            v,
            json!({"id": "ag_123", "slug": "team-greeter", "display_name": "Greeter"})
        );
        let back: AgentSummary = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn agent_summary_omits_none_display_name() {
        let a = AgentSummary {
            id: "ag_1".into(),
            slug: "s".into(),
            display_name: None,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v, json!({"id": "ag_1", "slug": "s"}));
    }

    #[test]
    fn env_var_spec_roundtrip() {
        let e = EnvVarSpec {
            name: "OPENAI_API_KEY".into(),
            secret_ref: "vault://openai/prod".into(),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v,
            json!({"name": "OPENAI_API_KEY", "secret_ref": "vault://openai/prod"})
        );
        let back: EnvVarSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn secret_mount_spec_with_mode() {
        let m = SecretMountSpec {
            mount_path: "/run/secrets/token".into(),
            secret_ref: "vault://t".into(),
            mode: Some(0o400),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["mode"], json!(0o400));
        let back: SecretMountSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn secret_mount_spec_omits_none_mode() {
        let m = SecretMountSpec {
            mount_path: "/x".into(),
            secret_ref: "vault://x".into(),
            mode: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("mode").is_none());
    }

    #[test]
    fn network_policy_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&NetworkPolicy::Deny).unwrap(),
            "\"deny\""
        );
        assert_eq!(
            serde_json::to_string(&NetworkPolicy::Allowlist).unwrap(),
            "\"allowlist\""
        );
        assert_eq!(
            serde_json::to_string(&NetworkPolicy::Open).unwrap(),
            "\"open\""
        );
    }

    #[test]
    fn network_policy_parses_lowercase() {
        assert_eq!(
            serde_json::from_str::<NetworkPolicy>("\"deny\"").unwrap(),
            NetworkPolicy::Deny
        );
        assert_eq!(
            serde_json::from_str::<NetworkPolicy>("\"allowlist\"").unwrap(),
            NetworkPolicy::Allowlist
        );
        assert_eq!(
            serde_json::from_str::<NetworkPolicy>("\"open\"").unwrap(),
            NetworkPolicy::Open
        );
    }

    #[test]
    fn container_provisioning_default() {
        let p = ContainerProvisioning::default();
        assert!(p.env.is_empty());
        assert!(p.mounts.is_empty());
        // Default derives Deny via `Default`-on-vec/Option; network_policy
        // default lives behind serde, so check by decoding `{}`.
        let p2: ContainerProvisioning = serde_json::from_str("{}").unwrap();
        assert_eq!(p2.network_policy, NetworkPolicy::Deny);
        assert!(p2.allowed_egress.is_empty());
    }

    #[test]
    fn container_provisioning_roundtrip_full() {
        let p = ContainerProvisioning {
            env: vec![EnvVarSpec {
                name: "K".into(),
                secret_ref: "vault://k".into(),
            }],
            mounts: vec![SecretMountSpec {
                mount_path: "/m".into(),
                secret_ref: "vault://m".into(),
                mode: Some(0o400),
            }],
            network_policy: NetworkPolicy::Allowlist,
            allowed_egress: vec!["api.example.com".into()],
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: ContainerProvisioning = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn container_provisioning_omits_empty_allowed_egress() {
        let p = ContainerProvisioning {
            env: vec![],
            mounts: vec![],
            network_policy: NetworkPolicy::Open,
            allowed_egress: vec![],
        };
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("allowed_egress").is_none());
    }

    #[test]
    fn pending_approval_roundtrip() {
        let a = PendingApproval {
            id: "apr_1".into(),
            agent_id: "ag_1".into(),
            action: "read_secret".into(),
            reason: Some("debug".into()),
            requested_at: "2026-05-20T12:00:00Z".into(),
        };
        let v = serde_json::to_value(&a).unwrap();
        let back: PendingApproval = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn pending_approval_omits_none_reason() {
        let a = PendingApproval {
            id: "apr_1".into(),
            agent_id: "ag_1".into(),
            action: "x".into(),
            reason: None,
            requested_at: "2026-05-20T12:00:00Z".into(),
        };
        let v = serde_json::to_value(&a).unwrap();
        assert!(v.get("reason").is_none());
    }

    #[test]
    fn default_network_policy_helper_is_deny() {
        assert_eq!(default_network_policy(), NetworkPolicy::Deny);
    }
}
