//! Self-modification module.
//!
//! Backs the `install_packages` and `add_mcp_server` MCP tools. The module
//! validates package names + transport JSON, produces a typed [`ChangeRequest`]
//! that the host applies (it updates `container_configs`, rebuilds the image,
//! and restarts the container).

use crate::context::{Module, ModuleContext};
use crate::error::ModuleError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

/// Which package manager a request targets.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Apt,
    Npm,
}

impl PackageManager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Npm => "npm",
        }
    }
}

/// Package validation errors.
#[derive(Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackageError {
    #[error("package name is empty")]
    Empty,
    #[error("package name `{0}` is too long")]
    TooLong(String),
    #[error("package name `{0}` contains illegal character `{1}`")]
    IllegalChar(String, char),
    #[error("package name `{0}` starts with an illegal character")]
    BadStart(String),
    #[error("scoped npm package `{0}` is missing the package name after `/`")]
    ScopeMissingName(String),
    #[error("mcp transport JSON is invalid: {0}")]
    BadTransport(String),
    #[error("mcp server name `{0}` contains illegal character `{1}`")]
    BadMcpName(String, char),
}

const MAX_PACKAGE_LEN: usize = 214;

/// Validate an apt package name (alnum, dash, plus, dot — must start alnum).
pub fn validate_apt_package(name: &str) -> Result<(), PackageError> {
    if name.is_empty() {
        return Err(PackageError::Empty);
    }
    if name.len() > MAX_PACKAGE_LEN {
        return Err(PackageError::TooLong(name.to_owned()));
    }
    let first = name.chars().next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(PackageError::BadStart(name.to_owned()));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.')) {
            return Err(PackageError::IllegalChar(name.to_owned(), c));
        }
    }
    Ok(())
}

/// Validate an npm package name (alnum, dash, dot, underscore, with optional
/// `@scope/` prefix).
pub fn validate_npm_package(name: &str) -> Result<(), PackageError> {
    if name.is_empty() {
        return Err(PackageError::Empty);
    }
    if name.len() > MAX_PACKAGE_LEN {
        return Err(PackageError::TooLong(name.to_owned()));
    }
    let (scope, bare) = if let Some(rest) = name.strip_prefix('@') {
        let mut parts = rest.splitn(2, '/');
        let scope = parts.next().unwrap_or("");
        let bare = parts.next().unwrap_or("");
        if bare.is_empty() {
            return Err(PackageError::ScopeMissingName(name.to_owned()));
        }
        if scope.is_empty() {
            return Err(PackageError::BadStart(name.to_owned()));
        }
        for c in scope.chars() {
            if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                return Err(PackageError::IllegalChar(name.to_owned(), c));
            }
        }
        (Some(scope), bare)
    } else {
        (None, name)
    };
    let first = bare.chars().next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(PackageError::BadStart(name.to_owned()));
    }
    for c in bare.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err(PackageError::IllegalChar(name.to_owned(), c));
        }
    }
    // Keep `scope` referenced so the lint passes.
    let _ = scope;
    Ok(())
}

/// Validate an MCP server name. Same charset as npm bare names (without scope).
pub fn validate_mcp_name(name: &str) -> Result<(), PackageError> {
    if name.is_empty() {
        return Err(PackageError::Empty);
    }
    if name.len() > MAX_PACKAGE_LEN {
        return Err(PackageError::TooLong(name.to_owned()));
    }
    let first = name.chars().next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(PackageError::BadStart(name.to_owned()));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return Err(PackageError::BadMcpName(name.to_owned(), c));
        }
    }
    Ok(())
}

/// Validate an MCP server transport JSON object. We accept either
/// `{"type": "stdio", "command": "...", "args": [...]}` or
/// `{"type": "http", "url": "https://..."}`. Anything else is rejected here so
/// downstream consumers don't have to guess.
pub fn validate_mcp_transport(transport: &serde_json::Value) -> Result<(), PackageError> {
    let obj = transport
        .as_object()
        .ok_or_else(|| PackageError::BadTransport("not a JSON object".into()))?;
    let ty = obj
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| PackageError::BadTransport("missing `type`".into()))?;
    match ty {
        "stdio" => {
            if !obj.contains_key("command") {
                return Err(PackageError::BadTransport("stdio missing `command`".into()));
            }
        }
        "http" | "http-sse" => {
            let url = obj
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| PackageError::BadTransport(format!("{ty} missing `url`")))?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(PackageError::BadTransport(format!(
                    "{ty} url must be http(s): {url}"
                )));
            }
        }
        other => {
            return Err(PackageError::BadTransport(format!(
                "unknown transport `{other}`"
            )))
        }
    }
    Ok(())
}

/// A typed change request the host applies. Self-mod actions never touch the
/// filesystem directly — they build one of these and hand it off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeRequest {
    InstallPackages {
        apt: Vec<String>,
        npm: Vec<String>,
        reason: String,
    },
    AddMcpServer {
        name: String,
        transport: serde_json::Value,
        reason: String,
    },
}

/// Module impl. Pure validator — no hooks registered.
pub struct SelfModModule;

impl Default for SelfModModule {
    fn default() -> Self {
        Self
    }
}

impl SelfModModule {
    pub fn build_install_packages(
        &self,
        apt: Vec<String>,
        npm: Vec<String>,
        reason: String,
    ) -> Result<ChangeRequest, PackageError> {
        for p in &apt {
            validate_apt_package(p)?;
        }
        for p in &npm {
            validate_npm_package(p)?;
        }
        Ok(ChangeRequest::InstallPackages { apt, npm, reason })
    }

    pub fn build_add_mcp_server(
        &self,
        name: String,
        transport: serde_json::Value,
        reason: String,
    ) -> Result<ChangeRequest, PackageError> {
        validate_mcp_name(&name)?;
        validate_mcp_transport(&transport)?;
        Ok(ChangeRequest::AddMcpServer {
            name,
            transport,
            reason,
        })
    }
}

#[async_trait]
impl Module for SelfModModule {
    fn name(&self) -> &'static str {
        "self_mod"
    }

    async fn install(&self, _ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        // No hooks: the host calls into this module's builders from the MCP
        // tool handlers.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockModuleContext;

    #[test]
    fn apt_accepts_normal_names() {
        for n in ["jq", "git", "lib-foo+", "python3.12"] {
            validate_apt_package(n).unwrap_or_else(|e| panic!("{n}: {e}"));
        }
    }

    #[test]
    fn apt_rejects_bad_names() {
        assert!(matches!(
            validate_apt_package("").unwrap_err(),
            PackageError::Empty
        ));
        assert!(matches!(
            validate_apt_package("-foo").unwrap_err(),
            PackageError::BadStart(_)
        ));
        assert!(matches!(
            validate_apt_package("foo bar").unwrap_err(),
            PackageError::IllegalChar(..)
        ));
        assert!(matches!(
            validate_apt_package("foo$bar").unwrap_err(),
            PackageError::IllegalChar(..)
        ));
        let long = "a".repeat(MAX_PACKAGE_LEN + 1);
        assert!(matches!(
            validate_apt_package(&long).unwrap_err(),
            PackageError::TooLong(_)
        ));
    }

    #[test]
    fn npm_accepts_bare_and_scoped() {
        for n in [
            "left-pad",
            "is-thirteen",
            "ts-node",
            "lodash.fp",
            "@scope/pkg",
            "@scope/pkg.subpkg",
        ] {
            validate_npm_package(n).unwrap_or_else(|e| panic!("{n}: {e}"));
        }
    }

    #[test]
    fn npm_rejects_bad_names() {
        assert!(matches!(
            validate_npm_package("").unwrap_err(),
            PackageError::Empty
        ));
        assert!(matches!(
            validate_npm_package(".bad").unwrap_err(),
            PackageError::BadStart(_)
        ));
        assert!(matches!(
            validate_npm_package("@scope/").unwrap_err(),
            PackageError::ScopeMissingName(_)
        ));
        assert!(matches!(
            validate_npm_package("@/pkg").unwrap_err(),
            PackageError::BadStart(_)
        ));
        assert!(matches!(
            validate_npm_package("@sc ope/pkg").unwrap_err(),
            PackageError::IllegalChar(..)
        ));
        assert!(matches!(
            validate_npm_package("hello world").unwrap_err(),
            PackageError::IllegalChar(..)
        ));
    }

    #[test]
    fn npm_too_long() {
        let long = "a".repeat(MAX_PACKAGE_LEN + 1);
        assert!(matches!(
            validate_npm_package(&long).unwrap_err(),
            PackageError::TooLong(_)
        ));
    }

    #[test]
    fn mcp_name_accepts_alnum() {
        for n in ["weather", "weather-bot", "weather_bot.v2"] {
            validate_mcp_name(n).unwrap_or_else(|e| panic!("{n}: {e}"));
        }
    }

    #[test]
    fn mcp_name_rejects_bad_chars() {
        assert!(matches!(
            validate_mcp_name("").unwrap_err(),
            PackageError::Empty
        ));
        assert!(matches!(
            validate_mcp_name(".x").unwrap_err(),
            PackageError::BadStart(_)
        ));
        assert!(matches!(
            validate_mcp_name("a b").unwrap_err(),
            PackageError::BadMcpName(..)
        ));
        let long = "a".repeat(MAX_PACKAGE_LEN + 1);
        assert!(matches!(
            validate_mcp_name(&long).unwrap_err(),
            PackageError::TooLong(_)
        ));
    }

    #[test]
    fn mcp_transport_stdio_ok() {
        validate_mcp_transport(&serde_json::json!({"type":"stdio","command":"x"})).unwrap();
    }

    #[test]
    fn mcp_transport_http_ok() {
        validate_mcp_transport(&serde_json::json!({"type":"http","url":"https://x"})).unwrap();
        validate_mcp_transport(&serde_json::json!({"type":"http-sse","url":"http://x"})).unwrap();
    }

    #[test]
    fn mcp_transport_bad_shape() {
        for v in [
            serde_json::json!("not-an-object"),
            serde_json::json!({}),
            serde_json::json!({"type":"weird"}),
            serde_json::json!({"type":"stdio"}),
            serde_json::json!({"type":"http"}),
            serde_json::json!({"type":"http","url":"ftp://x"}),
        ] {
            assert!(validate_mcp_transport(&v).is_err(), "expected err for {v}");
        }
    }

    #[test]
    fn build_install_validates() {
        let m = SelfModModule;
        let req = m
            .build_install_packages(
                vec!["jq".into()],
                vec!["left-pad".into()],
                "tests".into(),
            )
            .unwrap();
        if let ChangeRequest::InstallPackages { apt, npm, reason } = req {
            assert_eq!(apt, vec!["jq"]);
            assert_eq!(npm, vec!["left-pad"]);
            assert_eq!(reason, "tests");
        } else {
            panic!("expected InstallPackages variant");
        }
    }

    #[test]
    fn build_install_propagates_apt_err() {
        let m = SelfModModule;
        let err = m
            .build_install_packages(vec!["bad name".into()], vec![], "x".into())
            .unwrap_err();
        assert!(matches!(err, PackageError::IllegalChar(..)));
    }

    #[test]
    fn build_install_propagates_npm_err() {
        let m = SelfModModule;
        let err = m
            .build_install_packages(vec![], vec![".bad".into()], "x".into())
            .unwrap_err();
        assert!(matches!(err, PackageError::BadStart(_)));
    }

    #[test]
    fn build_add_mcp_validates() {
        let m = SelfModModule;
        let req = m
            .build_add_mcp_server(
                "weather".into(),
                serde_json::json!({"type":"stdio","command":"x"}),
                "reason".into(),
            )
            .unwrap();
        if let ChangeRequest::AddMcpServer { name, .. } = req {
            assert_eq!(name, "weather");
        } else {
            panic!("expected AddMcpServer variant");
        }
    }

    #[test]
    fn build_add_mcp_propagates_name_err() {
        let m = SelfModModule;
        let err = m
            .build_add_mcp_server(
                "bad name".into(),
                serde_json::json!({"type":"stdio","command":"x"}),
                "x".into(),
            )
            .unwrap_err();
        assert!(matches!(err, PackageError::BadMcpName(..)));
    }

    #[test]
    fn build_add_mcp_propagates_transport_err() {
        let m = SelfModModule;
        let err = m
            .build_add_mcp_server(
                "weather".into(),
                serde_json::json!({"type":"weird"}),
                "x".into(),
            )
            .unwrap_err();
        assert!(matches!(err, PackageError::BadTransport(_)));
    }

    #[test]
    fn change_request_serde_roundtrip() {
        let install = ChangeRequest::InstallPackages {
            apt: vec!["jq".into()],
            npm: vec!["pkg".into()],
            reason: "r".into(),
        };
        let mcp = ChangeRequest::AddMcpServer {
            name: "weather".into(),
            transport: serde_json::json!({"type":"stdio","command":"x"}),
            reason: "r".into(),
        };
        for r in [install, mcp] {
            let s = serde_json::to_string(&r).unwrap();
            let back: ChangeRequest = serde_json::from_str(&s).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn package_manager_as_str_roundtrip() {
        assert_eq!(PackageManager::Apt.as_str(), "apt");
        assert_eq!(PackageManager::Npm.as_str(), "npm");
        for pm in [PackageManager::Apt, PackageManager::Npm] {
            let s = serde_json::to_string(&pm).unwrap();
            let back: PackageManager = serde_json::from_str(&s).unwrap();
            assert_eq!(pm, back);
        }
    }

    #[test]
    fn package_error_serde_roundtrip() {
        for e in [
            PackageError::Empty,
            PackageError::TooLong("x".into()),
            PackageError::IllegalChar("x".into(), 'a'),
            PackageError::BadStart("x".into()),
            PackageError::ScopeMissingName("x".into()),
            PackageError::BadTransport("x".into()),
            PackageError::BadMcpName("x".into(), 'a'),
        ] {
            let s = serde_json::to_string(&e).unwrap();
            let back: PackageError = serde_json::from_str(&s).unwrap();
            assert_eq!(e, back);
        }
    }

    #[tokio::test]
    async fn install_is_noop() {
        let m = SelfModModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert!(ctx.registered().is_empty());
        assert_eq!(m.name(), "self_mod");
    }
}
