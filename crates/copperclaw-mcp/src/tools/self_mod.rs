//! Self-modification tools: `install_packages`, `add_mcp_server`.
//!
//! These tools never actually mutate the container themselves — they request
//! an approval. The runner translates the effect into an approval row.

pub mod install_packages {
    //! `install_packages`: install apt / npm / pip packages.
    //!
    //! Two scopes (M18 E1):
    //!
    //! - `scope: "image"` (default, unchanged): records the packages into the
    //!   group's pending `container_configs` and requests approval; they are
    //!   baked into the image on the *next* container spawn. Nothing changes in
    //!   the current session.
    //! - `scope: "session"`: runs the ecosystem-appropriate LOCAL install into
    //!   the session's persistent `/data` **now** (a venv for pip, a prefixed
    //!   global for npm) so the package is importable/runnable this very turn,
    //!   AND still records the bakeable ecosystems (apt/npm) for the next image
    //!   — "works now, permanent later". The local install runs in-container
    //!   (the tool executes here); if the container's deny-default egress
    //!   blocks the package registry, the tool error carries the exact
    //!   `cclaw groups config set-egress-allow` command an operator needs.
    //!
    //! `HOME=/data` in the container (see `spawn.rs`), and `/data` is the
    //! session's persistent volume, so the venv / npm-prefix survive container
    //! respawns for the life of the session. There is no image-level pip bake
    //! dimension, so pip is only accepted under `scope: "session"`.

    use crate::context::{InstallScope, InstallSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;
    use std::process::Stdio;
    use std::time::Duration;

    /// Persistent session venv used for pip installs (`HOME=/data`).
    const VENV_DIR: &str = "/data/.venv";
    /// Persistent npm prefix for `-g` installs.
    const NPM_PREFIX: &str = "/data/.npm-global";
    /// Shell line the agent runs in a later `shell` call to use the venv.
    const PIP_ACTIVATION: &str = "source /data/.venv/bin/activate";
    /// Shell line that puts the npm-global bin dir on PATH.
    const NPM_ACTIVATION: &str = "export PATH=/data/.npm-global/bin:$PATH";
    /// Wall-clock ceiling for a single local install step.
    const INSTALL_TIMEOUT: Duration = Duration::from_secs(240);
    /// Cap on captured stderr/stdout echoed back in an error.
    const DETAIL_CAP: usize = 1500;

    #[derive(Debug, Deserialize)]
    struct Input {
        #[serde(default)]
        apt: Vec<String>,
        #[serde(default)]
        npm: Vec<String>,
        #[serde(default)]
        pip: Vec<String>,
        reason: String,
        #[serde(default)]
        scope: InstallScope,
    }

    pub fn schema() -> Tool {
        make_tool(
            "install_packages",
            "Install apt / npm / pip packages. `scope:\"image\"` (default) bakes \
             them into the next container image (subject to approval). \
             `scope:\"session\"` ALSO installs pip/npm into the session's /data \
             right now so they work this turn — the default you want mid-build.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["reason"],
                "properties": {
                    "apt": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "apt packages. Always image-scoped (need root + build-time egress); baked at the next spawn even under scope:\"session\"."
                    },
                    "npm": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "npm packages. Under scope:\"session\" installed globally under /data/.npm-global now AND recorded for the next image."
                    },
                    "pip": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Python packages. ONLY valid with scope:\"session\" (there is no image-level pip bake); installed into /data/.venv now."
                    },
                    "reason": { "type": "string", "minLength": 1 },
                    "scope": {
                        "type": "string",
                        "enum": ["image", "session"],
                        "description": "\"image\" (default): bake into the next image. \"session\": also install pip/npm into /data now so they work this turn."
                    }
                }
            }),
        )
    }

    /// The two ack states the card calls out. `ImagePending` is emitted at
    /// tool-emit time for `scope: "image"` (nothing ran yet — awaiting approval
    /// then the next rebuild). `SessionDone` is emitted after the in-container
    /// install actually ran for `scope: "session"`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum InstallAck {
        /// Recorded for the next image build; nothing installed this session.
        ImagePending { apt: Vec<String>, npm: Vec<String> },
        /// Local install already ran; bakeables (npm/apt) also recorded.
        SessionDone {
            pip: Vec<String>,
            npm: Vec<String>,
            apt_deferred: Vec<String>,
        },
    }

    impl InstallAck {
        /// Shell lines the agent should run in a later `shell` call so the
        /// freshly-installed tools are on PATH / the venv is active. Empty for
        /// the image-pending state.
        pub(crate) fn activation_lines(&self) -> Vec<String> {
            match self {
                Self::ImagePending { .. } => Vec::new(),
                Self::SessionDone { pip, npm, .. } => {
                    let mut lines = Vec::new();
                    if !pip.is_empty() {
                        lines.push(PIP_ACTIVATION.to_owned());
                    }
                    if !npm.is_empty() {
                        lines.push(NPM_ACTIVATION.to_owned());
                    }
                    lines
                }
            }
        }

        /// Human-readable ack surfaced back to the model.
        pub(crate) fn message(&self) -> String {
            match self {
                Self::ImagePending { apt, npm } => {
                    let mut parts = Vec::new();
                    if !apt.is_empty() {
                        parts.push(format!("apt: {}", apt.join(", ")));
                    }
                    if !npm.is_empty() {
                        parts.push(format!("npm: {}", npm.join(", ")));
                    }
                    format!(
                        "Recorded for the NEXT image build (awaiting approval): {}. \
                         These do NOT apply to the current session — they install \
                         when the container next rebuilds. If you need a package \
                         right now, call install_packages again with scope:\"session\".",
                        parts.join("; ")
                    )
                }
                Self::SessionDone {
                    pip,
                    npm,
                    apt_deferred,
                } => {
                    let mut done = Vec::new();
                    if !pip.is_empty() {
                        done.push(format!("pip → {VENV_DIR}: {}", pip.join(", ")));
                    }
                    if !npm.is_empty() {
                        done.push(format!("npm → {NPM_PREFIX}: {}", npm.join(", ")));
                    }
                    let activation = self.activation_lines().join("` and `");
                    let mut msg = format!(
                        "Installed into /data NOW (usable this session): {}. \
                         Activate in a later shell call with `{activation}`.",
                        done.join("; ")
                    );
                    if !npm.is_empty() {
                        msg.push_str(
                            " The npm packages were also recorded for the next image \
                             so they persist to fresh sessions.",
                        );
                    }
                    if !apt_deferred.is_empty() {
                        msg.push_str(&format!(
                            " apt packages ({}) cannot be installed live (need root + \
                             build-time egress) — they were recorded for the next image \
                             build only.",
                            apt_deferred.join(", ")
                        ));
                    }
                    msg
                }
            }
        }
    }

    /// A failure from the in-container session install, classified so the
    /// handler can attach the right remediation.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum SessionInstallError {
        /// The registry was unreachable — deny-default egress (or offline).
        Egress {
            ecosystem: &'static str,
            detail: String,
        },
        /// The ecosystem's toolchain (`python3` / `npm`) is not in the image.
        ToolchainMissing {
            ecosystem: &'static str,
            program: String,
        },
        /// Any other install failure (bad package, disk, non-zero exit).
        Other {
            ecosystem: &'static str,
            detail: String,
        },
    }

    /// Registries a given ecosystem reaches, as `host:port` for the egress
    /// allow-list.
    pub(crate) fn registry_hosts(ecosystem: &str) -> &'static [&'static str] {
        match ecosystem {
            "pip" => &["pypi.org:443", "files.pythonhosted.org:443"],
            "npm" => &["registry.npmjs.org:443"],
            _ => &[],
        }
    }

    /// Build the `cclaw` egress-allow remediation for a blocked ecosystem.
    /// This is the NEW deny-default hint the card asks for — no such hint
    /// existed before (denial surfaced as a raw DNS/nftables failure).
    pub(crate) fn egress_allow_hint(ecosystem: &str) -> String {
        let mut allows = String::new();
        for h in registry_hosts(ecosystem) {
            allows.push_str(" --allow ");
            allows.push_str(h);
        }
        format!(
            "`{ecosystem}` could not reach its package registry — container egress is \
             denied by default. Ask an operator to allow the registry, then retry: \
             `cclaw groups config set-egress-allow <agent-group-id>{allows}`"
        )
    }

    /// Map a classified install failure to the tool error the model sees.
    pub(crate) fn session_error_to_tool_error(err: &SessionInstallError) -> ToolError {
        match err {
            SessionInstallError::Egress { ecosystem, detail } => ToolError::Context(format!(
                "session install failed. {} (registry error: {})",
                egress_allow_hint(ecosystem),
                truncate(detail)
            )),
            SessionInstallError::ToolchainMissing { ecosystem, program } => {
                ToolError::Context(format!(
                    "session `{ecosystem}` install failed: `{program}` is not in this image. \
                     Add the toolchain first with install_packages scope:\"image\" (e.g. apt \
                     python3-venv, or the node runtime), let it rebuild, then retry."
                ))
            }
            SessionInstallError::Other { ecosystem, detail } => ToolError::Context(format!(
                "session `{ecosystem}` install failed: {}",
                truncate(detail)
            )),
        }
    }

    /// Heuristic: does this pip/npm output look like a deny-default egress
    /// block (DNS resolution / connect failure) rather than a package error?
    pub(crate) fn looks_like_egress_denial(output: &str) -> bool {
        const SIGNATURES: &[&str] = &[
            "temporary failure in name resolution",
            "could not resolve host",
            "name or service not known",
            "network is unreachable",
            "no route to host",
            "connection timed out",
            "connection refused",
            "getaddrinfo",
            "eai_again",
            "econnrefused",
            "etimedout",
            "enetunreach",
            "failed to establish a new connection",
            "max retries exceeded",
            "network error",
        ];
        let low = output.to_ascii_lowercase();
        SIGNATURES.iter().any(|s| low.contains(s))
    }

    fn truncate(s: &str) -> String {
        let trimmed = s.trim();
        if trimmed.len() <= DETAIL_CAP {
            return trimmed.to_owned();
        }
        let mut end = DETAIL_CAP;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }

    /// The ordered subprocess steps for a pip session install: create/refresh
    /// the venv, then install into it. Pure so the argv is unit-testable
    /// without running anything.
    pub(crate) fn plan_pip_commands(pkgs: &[String]) -> Vec<(String, Vec<String>)> {
        let mut install = vec!["install".to_owned()];
        install.extend(pkgs.iter().cloned());
        vec![
            (
                "python3".to_owned(),
                vec!["-m".to_owned(), "venv".to_owned(), VENV_DIR.to_owned()],
            ),
            (format!("{VENV_DIR}/bin/pip"), install),
        ]
    }

    /// The subprocess step for an npm session install (global under the
    /// persistent prefix). Pure.
    pub(crate) fn plan_npm_commands(pkgs: &[String]) -> Vec<(String, Vec<String>)> {
        let mut args = vec![
            "install".to_owned(),
            "-g".to_owned(),
            "--prefix".to_owned(),
            NPM_PREFIX.to_owned(),
        ];
        args.extend(pkgs.iter().cloned());
        vec![("npm".to_owned(), args)]
    }

    /// Runs the local install for `scope: "session"`. Split behind a trait so
    /// unit tests can exercise the handler's emit/ack/error-mapping paths
    /// without a Docker container or network; the real subprocess path is
    /// covered by the Docker-gated integration test.
    #[async_trait::async_trait]
    pub(crate) trait SessionInstaller: Send + Sync {
        async fn install(&self, pip: &[String], npm: &[String]) -> Result<(), SessionInstallError>;
    }

    /// Production installer: actually shells out to pip/npm in the container.
    pub(crate) struct RealInstaller;

    impl RealInstaller {
        async fn run_step(
            program: &str,
            args: &[String],
            ecosystem: &'static str,
        ) -> Result<(), SessionInstallError> {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            // Mirror the container's runtime env so caches land under /data.
            cmd.env("HOME", "/data");
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SessionInstallError::ToolchainMissing {
                        ecosystem,
                        program: program.to_owned(),
                    });
                }
                Err(e) => {
                    return Err(SessionInstallError::Other {
                        ecosystem,
                        detail: format!("spawn failed: {e}"),
                    });
                }
            };

            let output = match tokio::time::timeout(INSTALL_TIMEOUT, child.wait_with_output()).await
            {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => {
                    return Err(SessionInstallError::Other {
                        ecosystem,
                        detail: format!("wait failed: {e}"),
                    });
                }
                Err(_) => {
                    return Err(SessionInstallError::Other {
                        ecosystem,
                        detail: format!(
                            "timed out after {}s (registry slow or blocked)",
                            INSTALL_TIMEOUT.as_secs()
                        ),
                    });
                }
            };

            if output.status.success() {
                return Ok(());
            }
            let combined = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if looks_like_egress_denial(&combined) {
                Err(SessionInstallError::Egress {
                    ecosystem,
                    detail: combined,
                })
            } else {
                Err(SessionInstallError::Other {
                    ecosystem,
                    detail: combined,
                })
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionInstaller for RealInstaller {
        async fn install(&self, pip: &[String], npm: &[String]) -> Result<(), SessionInstallError> {
            if !pip.is_empty() {
                for (prog, args) in plan_pip_commands(pip) {
                    Self::run_step(&prog, &args, "pip").await?;
                }
            }
            if !npm.is_empty() {
                for (prog, args) in plan_npm_commands(npm) {
                    Self::run_step(&prog, &args, "npm").await?;
                }
            }
            Ok(())
        }
    }

    fn validate(input: &Input) -> Result<(), ToolError> {
        if input.reason.trim().is_empty() {
            return Err(ToolError::Validation("`reason` must be non-empty".into()));
        }
        if input.apt.is_empty() && input.npm.is_empty() && input.pip.is_empty() {
            return Err(ToolError::Validation(
                "must request at least one apt, npm, or pip package".into(),
            ));
        }
        for pkg in input
            .apt
            .iter()
            .chain(input.npm.iter())
            .chain(input.pip.iter())
        {
            if pkg.trim().is_empty() {
                return Err(ToolError::Validation(
                    "package names must be non-empty".into(),
                ));
            }
        }
        if input.scope == InstallScope::Image && !input.pip.is_empty() {
            return Err(ToolError::Validation(
                "pip packages require scope:\"session\" — there is no image-level pip \
                 bake. Resubmit with scope:\"session\", or move them to apt/npm."
                    .into(),
            ));
        }
        Ok(())
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle_with(arguments, ctx, &RealInstaller).await
    }

    /// Inner handler parameterised over the installer so tests can inject a
    /// deterministic one (no container / network).
    pub(crate) async fn handle_with(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
        installer: &dyn SessionInstaller,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        validate(&input)?;

        match input.scope {
            InstallScope::Image => {
                let ack = InstallAck::ImagePending {
                    apt: input.apt.clone(),
                    npm: input.npm.clone(),
                };
                let spec = InstallSpec {
                    apt: input.apt,
                    npm: input.npm,
                    reason: input.reason,
                    scope: InstallScope::Image,
                };
                let effect = ctx
                    .emit_outbound(OutboundToolEffect::InstallPackages(spec))
                    .await?;
                Ok(success_json(&serde_json::json!({
                    "scope": "image",
                    "state": "pending_image_build",
                    "message": ack.message(),
                    "effect": effect,
                })))
            }
            InstallScope::Session => {
                // Works NOW: run the local install in-container. On egress
                // denial this returns the allow-list hint (correction 2).
                installer
                    .install(&input.pip, &input.npm)
                    .await
                    .map_err(|e| session_error_to_tool_error(&e))?;

                // Works LATER: record the bakeable ecosystems (apt/npm) so the
                // next image carries them. pip has no image dimension and lives
                // durably in /data, so it is intentionally not recorded.
                let recorded_for_image = !input.apt.is_empty() || !input.npm.is_empty();
                if recorded_for_image {
                    let spec = InstallSpec {
                        apt: input.apt.clone(),
                        npm: input.npm.clone(),
                        reason: input.reason.clone(),
                        scope: InstallScope::Session,
                    };
                    ctx.emit_outbound(OutboundToolEffect::InstallPackages(spec))
                        .await?;
                }

                let ack = InstallAck::SessionDone {
                    pip: input.pip,
                    npm: input.npm,
                    apt_deferred: input.apt,
                };
                Ok(success_json(&serde_json::json!({
                    "scope": "session",
                    "state": "installed_now",
                    "recorded_for_image": recorded_for_image,
                    "activation": ack.activation_lines(),
                    "message": ack.message(),
                })))
            }
        }
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }
    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod add_mcp_server {
    //! `add_mcp_server`: request the host to register a new MCP server for
    //! this agent.

    use crate::context::{AddMcpServerSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        name: String,
        transport: serde_json::Value,
        reason: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "add_mcp_server",
            "Request the host to add an MCP server. Transport shape is host-defined.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "transport", "reason"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "transport": { "type": "object" },
                    "reason": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.name.trim().is_empty() {
            return Err(ToolError::Validation("`name` must be non-empty".into()));
        }
        if input.reason.trim().is_empty() {
            return Err(ToolError::Validation("`reason` must be non-empty".into()));
        }
        if !input.transport.is_object() {
            return Err(ToolError::Validation(
                "`transport` must be an object".into(),
            ));
        }
        let spec = AddMcpServerSpec {
            name: input.name,
            transport: input.transport,
            reason: input.reason,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::AddMcpServer(spec))
            .await?;
        Ok(ack_to_result(&ack))
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }
    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::context::{MockToolContext, OutboundToolEffect};
    use crate::error::ToolError;
    use rmcp::model::JsonObject;
    use serde_json::Value;

    fn args_from(value: Value) -> Option<JsonObject> {
        match value {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    #[tokio::test]
    async fn install_happy_apt() {
        let ctx = MockToolContext::new();
        super::install_packages::handle(
            args_from(serde_json::json!({"apt": ["ripgrep"], "reason": "search"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::InstallPackages(s) => {
                assert_eq!(s.apt, vec!["ripgrep"]);
                assert!(s.npm.is_empty());
                assert_eq!(s.reason, "search");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn install_happy_npm() {
        let ctx = MockToolContext::new();
        super::install_packages::handle(
            args_from(serde_json::json!({"npm": ["typescript"], "reason": "build"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::InstallPackages(s) => {
                assert_eq!(s.npm, vec!["typescript"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn install_must_have_at_least_one_package() {
        let ctx = MockToolContext::new();
        let err = super::install_packages::handle(
            args_from(serde_json::json!({"reason": "nothing"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn install_blank_package_rejected() {
        let ctx = MockToolContext::new();
        let err = super::install_packages::handle(
            args_from(serde_json::json!({"apt": [" "], "reason": "r"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn install_blank_reason() {
        let ctx = MockToolContext::new();
        let err = super::install_packages::handle(
            args_from(serde_json::json!({"apt": ["x"], "reason": "  "})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    // ── E1: session-scope installs ────────────────────────────────────────
    use super::install_packages::{
        InstallAck, SessionInstallError, SessionInstaller, egress_allow_hint, handle_with,
        looks_like_egress_denial, plan_npm_commands, plan_pip_commands, registry_hosts,
        session_error_to_tool_error,
    };
    use crate::context::InstallScope;

    /// Deterministic installer for unit tests — never touches a container.
    struct MockInstaller(Result<(), SessionInstallError>);
    #[async_trait::async_trait]
    impl SessionInstaller for MockInstaller {
        async fn install(
            &self,
            _pip: &[String],
            _npm: &[String],
        ) -> Result<(), SessionInstallError> {
            self.0.clone()
        }
    }

    fn result_text(res: &rmcp::model::CallToolResult) -> String {
        res.content[0].as_text().unwrap().text.clone()
    }

    #[test]
    fn plan_pip_builds_venv_then_install() {
        let steps = plan_pip_commands(&["requests".into(), "rich".into()]);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].0, "python3");
        assert_eq!(steps[0].1, vec!["-m", "venv", "/data/.venv"]);
        assert_eq!(steps[1].0, "/data/.venv/bin/pip");
        assert_eq!(steps[1].1, vec!["install", "requests", "rich"]);
    }

    #[test]
    fn plan_npm_installs_global_under_prefix() {
        let steps = plan_npm_commands(&["typescript".into()]);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].0, "npm");
        assert_eq!(
            steps[0].1,
            vec![
                "install",
                "-g",
                "--prefix",
                "/data/.npm-global",
                "typescript"
            ]
        );
    }

    #[test]
    fn egress_denial_classifier() {
        for s in [
            "Could not resolve host: pypi.org",
            "Temporary failure in name resolution",
            "npm error network request to https://registry.npmjs.org failed, reason: getaddrinfo EAI_AGAIN",
            "Connection refused",
            "Max retries exceeded with url",
        ] {
            assert!(looks_like_egress_denial(s), "should flag: {s}");
        }
        for s in [
            "ERROR: Could not find a version that satisfies the requirement notapkg",
            "npm error 404 Not Found - GET https://registry.npmjs.org/notapkg",
        ] {
            assert!(!looks_like_egress_denial(s), "should NOT flag: {s}");
        }
    }

    #[test]
    fn egress_hint_names_registries_and_cclaw() {
        let pip = egress_allow_hint("pip");
        assert!(pip.contains("cclaw groups config set-egress-allow"));
        assert!(pip.contains("--allow pypi.org:443"));
        assert!(pip.contains("--allow files.pythonhosted.org:443"));
        let npm = egress_allow_hint("npm");
        assert!(npm.contains("--allow registry.npmjs.org:443"));
        assert_eq!(registry_hosts("apt"), &[] as &[&str]);
    }

    #[test]
    fn session_error_mapping_carries_hint() {
        let e = SessionInstallError::Egress {
            ecosystem: "pip",
            detail: "Could not resolve host".into(),
        };
        let ToolError::Context(msg) = session_error_to_tool_error(&e) else {
            panic!("expected Context error");
        };
        assert!(msg.contains("set-egress-allow"));
        assert!(msg.contains("pypi.org:443"));

        let e = SessionInstallError::ToolchainMissing {
            ecosystem: "pip",
            program: "python3".into(),
        };
        let ToolError::Context(msg) = session_error_to_tool_error(&e) else {
            panic!("expected Context error");
        };
        assert!(msg.contains("not in this image"));
        assert!(msg.contains("scope:\"image\""));
    }

    #[test]
    fn ack_image_pending_message_has_no_activation() {
        let ack = InstallAck::ImagePending {
            apt: vec!["jq".into()],
            npm: vec![],
        };
        assert!(ack.activation_lines().is_empty());
        let m = ack.message();
        assert!(m.contains("NEXT image build"));
        assert!(m.contains("scope:\"session\""));
    }

    #[test]
    fn ack_session_done_message_and_activation() {
        let ack = InstallAck::SessionDone {
            pip: vec!["requests".into()],
            npm: vec!["typescript".into()],
            apt_deferred: vec!["jq".into()],
        };
        let lines = ack.activation_lines();
        assert_eq!(
            lines,
            vec![
                "source /data/.venv/bin/activate".to_owned(),
                "export PATH=/data/.npm-global/bin:$PATH".to_owned(),
            ]
        );
        let m = ack.message();
        assert!(m.contains("Installed into /data NOW"));
        assert!(m.contains("recorded for the next image"));
        assert!(m.contains("apt packages (jq)"));
    }

    #[tokio::test]
    async fn image_scope_still_emits_and_defaults() {
        // No scope field → defaults to image, unchanged wire behaviour.
        let ctx = MockToolContext::new();
        let ok = MockInstaller(Ok(()));
        let res = handle_with(
            args_from(serde_json::json!({"apt": ["jq"], "npm": ["zod"], "reason": "r"})),
            &ctx,
            &ok,
        )
        .await
        .unwrap();
        assert_eq!(res.is_error, Some(false));
        match &ctx.calls()[0] {
            OutboundToolEffect::InstallPackages(s) => {
                assert_eq!(s.apt, vec!["jq"]);
                assert_eq!(s.npm, vec!["zod"]);
                assert_eq!(s.scope, InstallScope::Image);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_scope_rejects_pip() {
        let ctx = MockToolContext::new();
        let ok = MockInstaller(Ok(()));
        let err = handle_with(
            args_from(serde_json::json!({"pip": ["requests"], "reason": "r"})),
            &ctx,
            &ok,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.calls().is_empty(), "no effect on validation failure");
    }

    #[tokio::test]
    async fn session_scope_installs_now_and_records_bakeables() {
        // The "image config merge still happens" assertion at the tool layer:
        // a successful session install ALSO emits the InstallPackages effect
        // (scope=session) carrying the bakeable apt/npm the host merges.
        let ctx = MockToolContext::new();
        let ok = MockInstaller(Ok(()));
        let res = handle_with(
            args_from(serde_json::json!({
                "scope": "session", "pip": ["requests"], "npm": ["typescript"], "reason": "prototype"
            })),
            &ctx,
            &ok,
        )
        .await
        .unwrap();
        assert_eq!(res.is_error, Some(false));
        let text = result_text(&res);
        assert!(text.contains("installed_now"));
        assert!(text.contains("source /data/.venv/bin/activate"));
        // Bakeable npm recorded for the next image; pip is not (no bake dim).
        match &ctx.calls()[0] {
            OutboundToolEffect::InstallPackages(s) => {
                assert_eq!(s.npm, vec!["typescript"]);
                assert!(s.apt.is_empty());
                assert_eq!(s.scope, InstallScope::Session);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_scope_pip_only_records_nothing_for_image() {
        let ctx = MockToolContext::new();
        let ok = MockInstaller(Ok(()));
        let res = handle_with(
            args_from(serde_json::json!({
                "scope": "session", "pip": ["requests"], "reason": "r"
            })),
            &ctx,
            &ok,
        )
        .await
        .unwrap();
        assert_eq!(res.is_error, Some(false));
        // Nothing bakeable → no effect emitted; pip lives durably in /data.
        assert!(ctx.calls().is_empty());
        assert!(result_text(&res).contains("recorded_for_image"));
    }

    #[tokio::test]
    async fn session_scope_egress_failure_returns_hint_and_records_nothing() {
        let ctx = MockToolContext::new();
        let blocked = MockInstaller(Err(SessionInstallError::Egress {
            ecosystem: "pip",
            detail: "Could not resolve host: pypi.org".into(),
        }));
        let err = handle_with(
            args_from(serde_json::json!({
                "scope": "session", "pip": ["requests"], "npm": ["zod"], "reason": "r"
            })),
            &ctx,
            &blocked,
        )
        .await
        .unwrap_err();
        let ToolError::Context(msg) = err else {
            panic!("expected Context error");
        };
        assert!(msg.contains("set-egress-allow"));
        // A blocked install must not record a bake either.
        assert!(ctx.calls().is_empty());
    }

    #[test]
    fn install_schema_exposes_scope_and_pip() {
        let s = super::install_packages::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["reason"]));
        assert!(v["properties"]["pip"].is_object());
        assert_eq!(
            v["properties"]["scope"]["enum"],
            serde_json::json!(["image", "session"])
        );
    }

    /// Docker-gated end-to-end acceptance (separately-gated CI job — mirrors
    /// `copperclaw-providers/tests/ollama_live.rs`'s `#[ignore]` pattern).
    /// Runs the REAL installer, so it needs the session container's writable
    /// `/data`, `python3`/`npm`, and egress to the registries. Opt in with
    /// `cargo test -- --ignored session_install_docker_end_to_end`.
    #[tokio::test]
    #[ignore = "requires a session container (writable /data + python3/npm + egress); opt in with --ignored"]
    async fn session_install_docker_end_to_end() {
        let ctx = MockToolContext::new();
        let res = super::install_packages::handle(
            args_from(serde_json::json!({
                "scope": "session",
                "pip": ["cowsay"],
                "npm": ["is-thirteen"],
                "reason": "e1 acceptance"
            })),
            &ctx,
        )
        .await
        .expect("session install should succeed in the container");
        assert_eq!(res.is_error, Some(false));
    }

    #[tokio::test]
    async fn add_mcp_server_happy() {
        let ctx = MockToolContext::new();
        super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": "git",
                "transport": {"kind": "stdio", "cmd": "uvx", "args": ["mcp-server-git"]},
                "reason": "git ops"
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::AddMcpServer(s) => {
                assert_eq!(s.name, "git");
                assert_eq!(s.transport["kind"], "stdio");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_mcp_server_blank_name() {
        let ctx = MockToolContext::new();
        let err = super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": " ",
                "transport": {},
                "reason": "r"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn add_mcp_server_blank_reason() {
        let ctx = MockToolContext::new();
        let err = super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": "git",
                "transport": {},
                "reason": ""
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn add_mcp_server_transport_not_object() {
        let ctx = MockToolContext::new();
        let err = super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": "git",
                "transport": "stdio",
                "reason": "r"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn schemas_have_required() {
        let s = super::install_packages::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["reason"]));

        let s = super::add_mcp_server::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(
            v["required"],
            serde_json::json!(["name", "transport", "reason"])
        );
    }
}
