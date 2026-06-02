//! Shared error type for module installation and runtime hooks.

use thiserror::Error;

/// Errors that a module can produce during `install` or while running.
#[derive(Debug, Error)]
pub enum ModuleError {
    /// A module's configuration was invalid.
    #[error("invalid configuration for module `{module}`: {reason}")]
    InvalidConfig {
        module: &'static str,
        reason: String,
    },

    /// A required hook was already registered by another module and the host
    /// does not allow more than one provider for that hook.
    #[error("hook `{hook}` already registered (conflicting providers)")]
    HookConflict { hook: &'static str },

    /// The module needed a host capability that the running host does not
    /// provide. The host should fail boot in this case.
    #[error("module `{module}` requires capability `{capability}`")]
    MissingCapability {
        module: &'static str,
        capability: &'static str,
    },

    /// A free-form error condition with a static label, used by modules whose
    /// failure modes don't merit a dedicated variant.
    #[error("module `{module}` failed: {reason}")]
    Other {
        module: &'static str,
        reason: String,
    },
}

impl ModuleError {
    pub fn invalid_config(module: &'static str, reason: impl Into<String>) -> Self {
        Self::InvalidConfig {
            module,
            reason: reason.into(),
        }
    }

    pub fn other(module: &'static str, reason: impl Into<String>) -> Self {
        Self::Other {
            module,
            reason: reason.into(),
        }
    }

    pub fn module(&self) -> &'static str {
        match self {
            Self::InvalidConfig { module, .. }
            | Self::MissingCapability { module, .. }
            | Self::Other { module, .. } => module,
            Self::HookConflict { .. } => "<hook-conflict>",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_config_formats() {
        let err = ModuleError::invalid_config("typing", "interval_ms is zero");
        assert!(err.to_string().contains("typing"));
        assert!(err.to_string().contains("interval_ms is zero"));
        assert_eq!(err.module(), "typing");
    }

    #[test]
    fn hook_conflict_formats() {
        let err = ModuleError::HookConflict {
            hook: "access_gate",
        };
        assert!(err.to_string().contains("access_gate"));
        assert_eq!(err.module(), "<hook-conflict>");
    }

    #[test]
    fn missing_capability_formats() {
        let err = ModuleError::MissingCapability {
            module: "approvals",
            capability: "pending_approvals_db",
        };
        assert!(err.to_string().contains("approvals"));
        assert!(err.to_string().contains("pending_approvals_db"));
        assert_eq!(err.module(), "approvals");
    }

    #[test]
    fn other_formats() {
        let err = ModuleError::other("scheduling", "could not start ticker");
        assert!(err.to_string().contains("scheduling"));
        assert_eq!(err.module(), "scheduling");
    }
}
