//! Step 6 — Anthropic API key capture.
//!
//! Reads `ANTHROPIC_API_KEY` from the environment or prompts for it, then
//! writes a `.env` file inside the data directory with mode `0o600`. The
//! file also carries the `COPPERCLAW_DATA_DIR` and `CCLAW_SOCKET` pointers
//! that the host and admin client expect, so the README's quickstart
//! (`copperclaw --env-file <.env> run` / `cclaw groups list`) works without
//! the user having to re-export anything by hand.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use std::path::{Path, PathBuf};

/// Inputs the `.env` writer needs to wire setup to the host runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileSpec {
    /// API key persisted as `ANTHROPIC_API_KEY=...`.
    pub anthropic_api_key: String,
    /// Optional override base URL. When non-empty written as
    /// `ANTHROPIC_BASE_URL=...`. Used to route through
    /// Anthropic-API-compatible gateways like `OpenRouter`.
    pub anthropic_base_url: String,
    /// Value of `COPPERCLAW_DATA_DIR` — the dir that holds `copperclaw.db`,
    /// `sessions/`, and the cclaw socket. Empty to omit the line.
    pub data_dir: PathBuf,
    /// Value of `CCLAW_SOCKET` — the socket the host listens on. Empty to
    /// omit the line.
    pub cclaw_socket: PathBuf,
    /// Value of `COPPERCLAW_DEFAULT_IMAGE_TAG` — the sha-pinned tag of
    /// the session image setup just built. The host's container
    /// manager uses this when an agent group has no
    /// `container_config.image_tag` of its own. Empty to omit.
    pub default_image_tag: String,
    /// Value of `COPPERCLAW_SKILLS_DIR` — the directory of `SKILL.md`
    /// bundles auto-loaded into the agent system prompt at container
    /// spawn. Empty to omit the line (no skills are injected).
    pub skills_dir: PathBuf,
    /// Value of `COPPERCLAW_GROUPS_DIR` — the per-agent-group override
    /// root scanned alongside `skills_dir` (skills with matching
    /// names shadow the global ones). Empty to omit.
    pub groups_dir: PathBuf,
}

/// `OpenRouter`'s Anthropic-compatible base URL. The runner's
/// `AnthropicProvider` strips trailing `/v1` so this works verbatim.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Recognize friendly shortcuts in the base-URL prompt and expand
/// them to the real URLs (or to "" for the Anthropic default).
#[must_use]
pub fn expand_provider_shortcut(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "anthropic" | "default" => String::new(),
        "openrouter" | "or" => OPENROUTER_BASE_URL.to_string(),
        _ => raw.trim().to_string(),
    }
}

/// Step implementation.
#[derive(Debug, Default)]
pub struct AuthStep;

impl Step for AuthStep {
    fn name(&self) -> &'static str {
        "auth"
    }

    fn description(&self) -> &'static str {
        "Capture and persist the Anthropic API key"
    }

    fn is_skippable(&self) -> bool {
        false
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => prompt.secret("ANTHROPIC_API_KEY", "Anthropic API key")?,
        };
        // Optional override base URL — captured from the process env
        // or a setup-level env var so headless installs targeting
        // OpenRouter / a proxy can configure it in one shot. Falls
        // back to empty (omit the line) so existing installs stay
        // unchanged.
        //
        // We also expand the friendly shortcut `openrouter` to the
        // OpenRouter base URL so the operator doesn't have to type or
        // paste it — `anthropic` collapses to empty (use the default
        // Anthropic API).
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                prompt
                    .input(
                        "ANTHROPIC_BASE_URL",
                        "Provider base URL — type `openrouter`, `anthropic`, or paste a custom https:// URL (blank = anthropic)",
                        Some(""),
                    )
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            })
            .map(|raw| expand_provider_shortcut(&raw))
            .unwrap_or_default();
        let env_path = cfg.data_dir.join(".env");
        let host_data_dir = if cfg.central_db_path.as_os_str().is_empty() {
            cfg.data_dir.join("data")
        } else {
            cfg.central_db_path
                .parent()
                .map_or_else(|| cfg.data_dir.join("data"), Path::to_path_buf)
        };
        let cclaw_socket = host_data_dir.join("cclaw.sock");
        // Default `COPPERCLAW_SKILLS_DIR` to `<data_dir>/skills` and
        // `COPPERCLAW_GROUPS_DIR` to `<data_dir>/groups`. Operators can
        // override either by exporting the env var before running
        // setup. An empty path skips the line entirely so an unset
        // override means "don't write it".
        let skills_dir = std::env::var("COPPERCLAW_SKILLS_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| host_data_dir.join("skills"), PathBuf::from);
        let groups_dir = std::env::var("COPPERCLAW_GROUPS_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| host_data_dir.join("groups"), PathBuf::from);
        let spec = EnvFileSpec {
            anthropic_api_key: key,
            anthropic_base_url: base_url,
            data_dir: host_data_dir,
            cclaw_socket,
            default_image_tag: cfg.image_tag.clone(),
            skills_dir,
            groups_dir,
        };
        write_env_file(&env_path, &spec)?;
        cfg.env_file.clone_from(&env_path);
        Ok(StepResult::ok(format!(
            "wrote {} (0600)",
            env_path.display()
        )))
    }
}

/// Write the `.env` body described by `spec`.
///
/// The file is created with mode `0o600` from the start on Unix — there
/// is no window where the bytes are on disk under the default umask
/// (typically `0o644`, world-readable) before a follow-up chmod
/// tightens them. This closes a TOCTOU race during initial install
/// where a local reader could grab the freshly written API key.
pub fn write_env_file(path: &Path, spec: &EnvFileSpec) -> Result<(), StepError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = render_env_file(spec);
    crate::state::write_secret_file(path, contents.as_bytes())?;
    Ok(())
}

/// Render the `.env` body for `spec`. Keys with empty values are omitted.
#[must_use]
pub fn render_env_file(spec: &EnvFileSpec) -> String {
    let mut out = String::new();
    out.push_str(&format!("ANTHROPIC_API_KEY={}\n", spec.anthropic_api_key));
    if !spec.anthropic_base_url.is_empty() {
        out.push_str(&format!("ANTHROPIC_BASE_URL={}\n", spec.anthropic_base_url));
    }
    if !spec.data_dir.as_os_str().is_empty() {
        out.push_str(&format!(
            "COPPERCLAW_DATA_DIR={}\n",
            spec.data_dir.display()
        ));
    }
    if !spec.cclaw_socket.as_os_str().is_empty() {
        out.push_str(&format!("CCLAW_SOCKET={}\n", spec.cclaw_socket.display()));
    }
    if !spec.default_image_tag.is_empty() {
        out.push_str(&format!(
            "COPPERCLAW_DEFAULT_IMAGE_TAG={}\n",
            spec.default_image_tag
        ));
    }
    if !spec.skills_dir.as_os_str().is_empty() {
        out.push_str(&format!(
            "COPPERCLAW_SKILLS_DIR={}\n",
            spec.skills_dir.display()
        ));
    }
    if !spec.groups_dir.as_os_str().is_empty() {
        out.push_str(&format!(
            "COPPERCLAW_GROUPS_DIR={}\n",
            spec.groups_dir.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    fn spec(key: &str) -> EnvFileSpec {
        EnvFileSpec {
            anthropic_api_key: key.into(),
            anthropic_base_url: String::new(),
            data_dir: PathBuf::from("/srv/iron/data"),
            cclaw_socket: PathBuf::from("/srv/iron/data/cclaw.sock"),
            default_image_tag: String::new(),
            skills_dir: PathBuf::new(),
            groups_dir: PathBuf::new(),
        }
    }

    #[test]
    fn render_env_file_includes_key_and_paths() {
        let s = render_env_file(&spec("sk-abc"));
        assert!(s.contains("ANTHROPIC_API_KEY=sk-abc\n"));
        assert!(s.contains("COPPERCLAW_DATA_DIR=/srv/iron/data\n"));
        assert!(s.contains("CCLAW_SOCKET=/srv/iron/data/cclaw.sock\n"));
    }

    #[test]
    fn render_env_file_omits_empty_paths() {
        let s = render_env_file(&EnvFileSpec {
            anthropic_api_key: "sk".into(),
            anthropic_base_url: String::new(),
            data_dir: PathBuf::new(),
            cclaw_socket: PathBuf::new(),
            default_image_tag: String::new(),
            skills_dir: PathBuf::new(),
            groups_dir: PathBuf::new(),
        });
        assert_eq!(s, "ANTHROPIC_API_KEY=sk\n");
    }

    #[test]
    fn render_env_file_writes_skills_and_groups_dir() {
        let mut s = spec("sk");
        s.skills_dir = PathBuf::from("/srv/iron/data/skills");
        s.groups_dir = PathBuf::from("/srv/iron/data/groups");
        let body = render_env_file(&s);
        assert!(
            body.contains("COPPERCLAW_SKILLS_DIR=/srv/iron/data/skills\n"),
            "body: {body}"
        );
        assert!(
            body.contains("COPPERCLAW_GROUPS_DIR=/srv/iron/data/groups\n"),
            "body: {body}"
        );
    }

    #[test]
    fn render_env_file_includes_base_url_when_set() {
        let mut s = spec("sk");
        s.anthropic_base_url = "https://openrouter.ai/api/v1".into();
        let body = render_env_file(&s);
        assert!(
            body.contains("ANTHROPIC_BASE_URL=https://openrouter.ai/api/v1\n"),
            "body: {body}"
        );
    }

    #[test]
    fn write_env_file_creates_with_restricted_perms() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        write_env_file(&path, &spec("sk-xyz")).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=sk-xyz"));
        assert!(body.contains("COPPERCLAW_DATA_DIR=/srv/iron/data"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_env_file_sets_mode_0600_from_creation() {
        // Pinned regression: previously the file was created with the
        // default umask (typically 0o644) and chmod'd to 0o600 in a
        // second syscall — a brief window where any local user could
        // read the secret. The helper used here opens with `mode(0o600)`
        // so the file never exists with looser bits.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        write_env_file(&path, &spec("sk-pinned")).unwrap();
        let perms = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_env_file_tightens_perms_when_path_pre_exists_loose() {
        // Idempotent re-runs: if an older install left .env at 0o644,
        // a fresh write must converge to 0o600.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, b"stale\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();
        write_env_file(&path, &spec("sk-rerun")).unwrap();
        let after = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(after.mode() & 0o777, 0o600);
    }

    #[test]
    fn write_env_file_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("nested/.env");
        write_env_file(&nested, &spec("sk-1")).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn step_prompts_when_env_var_missing() {
        // The harness's parent env shouldn't define ANTHROPIC_API_KEY for
        // this test; if it does, exit early without flagging a failure.
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            return;
        }
        let dir = tempdir().unwrap();
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            central_db_path: dir.path().join("data/copperclaw.db"),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("ANTHROPIC_API_KEY", "sk-from-prompt");
        let res = AuthStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        let written = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(written.contains("sk-from-prompt"));
        assert!(written.contains(&format!(
            "COPPERCLAW_DATA_DIR={}\n",
            dir.path().join("data").display()
        )));
        assert!(written.contains(&format!(
            "CCLAW_SOCKET={}\n",
            dir.path().join("data/cclaw.sock").display()
        )));
    }

    #[test]
    fn step_metadata() {
        let s = AuthStep;
        assert_eq!(s.name(), "auth");
        assert!(!s.description().is_empty());
        assert!(!s.is_skippable());
    }

    #[test]
    fn expand_openrouter_shortcut() {
        assert_eq!(expand_provider_shortcut("openrouter"), OPENROUTER_BASE_URL);
        assert_eq!(expand_provider_shortcut("OpenRouter"), OPENROUTER_BASE_URL);
        assert_eq!(
            expand_provider_shortcut("  openrouter  "),
            OPENROUTER_BASE_URL
        );
        assert_eq!(expand_provider_shortcut("or"), OPENROUTER_BASE_URL);
    }

    #[test]
    fn expand_anthropic_shortcut_clears() {
        assert_eq!(expand_provider_shortcut("anthropic"), "");
        assert_eq!(expand_provider_shortcut("Anthropic"), "");
        assert_eq!(expand_provider_shortcut("default"), "");
        assert_eq!(expand_provider_shortcut(""), "");
        assert_eq!(expand_provider_shortcut("   "), "");
    }

    #[test]
    fn expand_passes_through_real_urls() {
        assert_eq!(
            expand_provider_shortcut("https://my-proxy.example.com/v1"),
            "https://my-proxy.example.com/v1"
        );
        // Trimmed.
        assert_eq!(
            expand_provider_shortcut("  https://x.example.com  "),
            "https://x.example.com"
        );
    }
}
