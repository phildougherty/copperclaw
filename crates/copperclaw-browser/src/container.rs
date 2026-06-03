//! Dedicated browser child-container spec construction (Phase 5a).
//!
//! The headless Chromium runs in a DEDICATED CHILD CONTAINER, distinct from
//! the agent's session container, with three security properties this module
//! constructs (and exhaustively unit-tests):
//!
//!   1. **OFF the broker trust domain.** The agent session container carries a
//!      per-session broker capability token in the `ANTHROPIC_API_KEY` env
//!      slot (it is how the runner reaches the model through the host-side
//!      broker). The browser child MUST NOT carry that token (nor any other
//!      session secret) — a renderer RCE on a malicious page must not be able
//!      to read another session's broker token. We build the child env from
//!      an explicit allow-list and assert no `ANTHROPIC_*` / token slot leaks
//!      in.
//!   2. **Egress-restricted.** The child spawns under
//!      [`EgressMode::DenyDefault`] with an allow-list limited to the single
//!      navigation target (host:port). It never gets the agent's broad egress.
//!   3. **Stronger sandbox requested.** The child attaches a hardened
//!      [`SandboxProfile`] (gVisor / Kata / Firecracker, falling back to a
//!      hardened `runc` floor) so a renderer escape lands in a stronger
//!      isolation boundary, not directly on the host kernel.
//!
//! The spec construction is pure and tested here. The privileged *spawn* of
//! this spec is the runtime path, gated behind the opt-in flag
//! ([`BrowserToolConfig::enabled`]).

use copperclaw_container_rt::{
    ContainerSpec, EgressMode, SandboxProfile, SandboxRuntime, select_sandbox_runtime,
};

/// Env-var names that must NEVER appear in the browser child container — the
/// broker token slot and other session secrets. The child is built from an
/// allow-list, so this list is the *assertion surface*: tests confirm none of
/// these is present, and [`browser_env`] never adds them.
pub const FORBIDDEN_ENV_KEYS: &[&str] = &[
    // The broker capability token rides this slot in the session container.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    // Search-provider keys the session may forward.
    "TAVILY_API_KEY",
    "EXA_API_KEY",
    "BRAVE_SEARCH_API_KEY",
    "SERPAPI_API_KEY",
    // Any explicit broker token env (defensive — not currently injected by
    // name, but listed so a future rename can't silently leak it here).
    "COPPERCLAW_BROKER_TOKEN",
];

/// Opt-in configuration for the browser tool. **Default is OFF** — with
/// `enabled == false` the tool refuses to run and no child container is ever
/// spawned, so default deployments have zero behaviour change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserToolConfig {
    /// Master opt-in switch. `false` (default) disables the tool entirely.
    pub enabled: bool,
    /// Image reference for the headless-browser child container.
    pub image: String,
    /// The sandbox runtime to *request*. Resolved against host availability
    /// at spawn by [`select_sandbox_runtime`]; falls back to the hardened
    /// `runc` floor when the requested microVM/gVisor runtime is absent.
    pub requested_runtime: SandboxRuntime,
}

impl Default for BrowserToolConfig {
    fn default() -> Self {
        Self {
            // OFF by default — the whole feature is opt-in.
            enabled: false,
            image: "copperclaw-browser:latest".to_string(),
            // Request gVisor by default: broad host support, strong isolation,
            // and it degrades to hardened-runc when `runsc` isn't installed.
            requested_runtime: SandboxRuntime::Runsc,
        }
    }
}

impl BrowserToolConfig {
    /// Enabled config with a custom image + requested runtime.
    #[must_use]
    pub fn enabled(image: impl Into<String>, requested_runtime: SandboxRuntime) -> Self {
        Self {
            enabled: true,
            image: image.into(),
            requested_runtime,
        }
    }

    /// Gating check: `Ok(())` when the tool may run, [`crate::BrowserError::Disabled`]
    /// otherwise. The single chokepoint every caller passes through before any
    /// container is provisioned.
    pub fn ensure_enabled(&self) -> Result<(), crate::BrowserError> {
        if self.enabled {
            Ok(())
        } else {
            Err(crate::BrowserError::Disabled)
        }
    }
}

/// Parameters for building one browser child-container spec.
#[derive(Debug, Clone)]
pub struct BrowserContainerParams<'a> {
    /// Container name (host-unique, e.g. `copperclaw-browser-<session>`).
    pub name: &'a str,
    /// Install slug, used for the orphan-cleanup label.
    pub install_slug: &'a str,
    /// The resolved egress allow-list — typically just the navigation
    /// target's `host:port`. The child gets nothing broader.
    pub egress_allow: Vec<String>,
    /// Which sandbox runtimes the host probed as available (the hardened-runc
    /// floor is always implicitly available and need not be listed).
    pub available_runtimes: &'a [SandboxRuntime],
}

/// Build the dedicated browser child-container spec from the tool config + per
/// call params. Pure: no I/O, fully unit-tested. The returned spec is what the
/// runtime path spawns (under the opt-in gate).
///
/// Security properties baked in (see the module docs):
///   * env carries ONLY the non-secret browser knobs (no broker token / no
///     `ANTHROPIC_*` / no provider keys);
///   * [`EgressMode::DenyDefault`] with the narrow allow-list;
///   * a hardened [`SandboxProfile`] requesting the strongest *available*
///     runtime no weaker than the configured request.
#[must_use]
pub fn build_browser_container_spec(
    cfg: &BrowserToolConfig,
    params: &BrowserContainerParams<'_>,
) -> ContainerSpec {
    let resolved_runtime = select_sandbox_runtime(cfg.requested_runtime, params.available_runtimes);
    let sandbox = SandboxProfile::hardened(resolved_runtime);

    let mut spec = ContainerSpec::new(params.name, &cfg.image)
        // Label so the host's orphan sweep finds + reaps it like any other
        // session container.
        .with_label("copperclaw.install", params.install_slug)
        .with_label("copperclaw.role", "browser")
        // Run as an unprivileged user inside the container; combined with the
        // userns remap in the sandbox profile, root-in-container is an
        // unprivileged host uid.
        .with_user("65534:65534")
        // Egress: deny-default, narrowed to the navigation target only.
        .with_egress_mode(EgressMode::DenyDefault)
        .with_egress_allow(params.egress_allow.clone())
        // Stronger sandbox requested (resolved against host availability).
        .with_sandbox(sandbox);

    // Env: explicit allow-list of NON-SECRET browser knobs only. Crucially we
    // never copy the session's env — the broker token and provider keys stay
    // out of this container entirely.
    for (k, v) in browser_env() {
        spec = spec.with_env(k, v);
    }

    spec
}

/// The complete, non-secret env for the browser child. An explicit allow-list:
/// the only way a secret reaches this container is if it is added here, and
/// [`FORBIDDEN_ENV_KEYS`] + the tests guard against that.
#[must_use]
pub fn browser_env() -> Vec<(&'static str, &'static str)> {
    vec![
        // Headless Chromium expects a HOME for its profile/cache.
        ("HOME", "/tmp/browser"),
        // Marker so anything inside knows it is the locked-down browser child,
        // not the agent session.
        ("COPPERCLAW_BROWSER_CHILD", "1"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(allow: Vec<String>, avail: &[SandboxRuntime]) -> BrowserContainerParams<'_> {
        BrowserContainerParams {
            name: "copperclaw-browser-sess1",
            install_slug: "slug",
            egress_allow: allow,
            available_runtimes: avail,
        }
    }

    // ── opt-in gating ────────────────────────────────────────────────────

    #[test]
    fn config_default_is_disabled() {
        let cfg = BrowserToolConfig::default();
        assert!(!cfg.enabled, "browser tool must be OFF by default");
        assert!(matches!(
            cfg.ensure_enabled(),
            Err(crate::BrowserError::Disabled)
        ));
    }

    #[test]
    fn config_enabled_passes_gate() {
        let cfg = BrowserToolConfig::enabled("img:tag", SandboxRuntime::Kata);
        assert!(cfg.enabled);
        assert!(cfg.ensure_enabled().is_ok());
        assert_eq!(cfg.requested_runtime, SandboxRuntime::Kata);
    }

    // ── no broker token / no secrets in the child env ────────────────────

    #[test]
    fn child_env_contains_no_forbidden_keys() {
        let env = browser_env();
        for (k, _) in &env {
            assert!(
                !FORBIDDEN_ENV_KEYS.contains(k),
                "browser child env leaked a forbidden key: {k}"
            );
        }
    }

    #[test]
    fn spec_env_has_no_broker_token_slot() {
        let cfg = BrowserToolConfig::enabled("img", SandboxRuntime::Runsc);
        let spec = build_browser_container_spec(&cfg, &params(vec!["example.com:443".into()], &[]));
        let keys: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        // The broker token rides ANTHROPIC_API_KEY in the session container —
        // it must be entirely absent here.
        for forbidden in FORBIDDEN_ENV_KEYS {
            assert!(
                !keys.contains(forbidden),
                "browser child spec carried forbidden env `{forbidden}`: {keys:?}"
            );
        }
        // Positive: the non-secret markers ARE present.
        assert!(keys.contains(&"COPPERCLAW_BROWSER_CHILD"));
        assert!(keys.contains(&"HOME"));
    }

    // ── egress restriction ───────────────────────────────────────────────

    #[test]
    fn spec_is_egress_deny_default_with_narrow_allow() {
        let cfg = BrowserToolConfig::enabled("img", SandboxRuntime::Runsc);
        let spec =
            build_browser_container_spec(&cfg, &params(vec!["target.example:443".into()], &[]));
        assert_eq!(spec.egress_mode, EgressMode::DenyDefault);
        assert_eq!(spec.egress_allow, vec!["target.example:443".to_string()]);
    }

    #[test]
    fn spec_egress_allow_is_only_the_target() {
        // The child must not inherit a broad allow-list; it gets exactly what
        // the caller scoped (the single navigation target here).
        let cfg = BrowserToolConfig::enabled("img", SandboxRuntime::Runsc);
        let spec = build_browser_container_spec(&cfg, &params(vec!["a.test:443".into()], &[]));
        assert_eq!(spec.egress_allow.len(), 1);
    }

    // ── stronger sandbox requested + resolved ────────────────────────────

    #[test]
    fn spec_requests_stronger_sandbox() {
        let cfg = BrowserToolConfig::enabled("img", SandboxRuntime::Runsc);
        // gVisor available → resolved to gVisor, hardened floor applied.
        let spec = build_browser_container_spec(
            &cfg,
            &params(vec!["a:443".into()], &[SandboxRuntime::Runsc]),
        );
        let sb = spec.sandbox.expect("browser child must request a sandbox");
        assert_eq!(sb.runtime, SandboxRuntime::Runsc);
        assert!(sb.cap_drop_all);
        assert!(sb.no_new_privileges);
        assert_eq!(sb.userns_remap.as_deref(), Some("copperclaw-browser"));
    }

    #[test]
    fn spec_sandbox_falls_back_to_hardened_runc_when_unavailable() {
        let cfg = BrowserToolConfig::enabled("img", SandboxRuntime::Firecracker);
        // Nothing installed → the hardened-runc floor, NOT a silent drop to
        // an unsandboxed runtime.
        let spec = build_browser_container_spec(&cfg, &params(vec!["a:443".into()], &[]));
        let sb = spec.sandbox.unwrap();
        assert_eq!(sb.runtime, SandboxRuntime::HardenedRunc);
        // The floor still applies the full hardening.
        assert!(sb.cap_drop_all);
        assert!(sb.no_new_privileges);
    }

    #[test]
    fn spec_runs_as_unprivileged_user() {
        let cfg = BrowserToolConfig::enabled("img", SandboxRuntime::Runsc);
        let spec = build_browser_container_spec(&cfg, &params(vec![], &[]));
        assert_eq!(spec.user.as_deref(), Some("65534:65534"));
    }

    #[test]
    fn spec_is_labelled_for_orphan_sweep() {
        let cfg = BrowserToolConfig::enabled("img", SandboxRuntime::Runsc);
        let spec = build_browser_container_spec(&cfg, &params(vec![], &[]));
        assert_eq!(
            spec.labels.get("copperclaw.install").map(String::as_str),
            Some("slug")
        );
        assert_eq!(
            spec.labels.get("copperclaw.role").map(String::as_str),
            Some("browser")
        );
    }
}
