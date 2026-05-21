//! Host configuration.
//!
//! The host is configured exclusively through environment variables (the
//! optional `.env` file is loaded with `dotenvy` before this struct is
//! parsed). Every field has a sensible default so an unconfigured invocation
//! still boots; the user's `setup` (T10) writes the env file at install
//! time.
//!
//! Supported env vars:
//!
//! | Var | Default | Meaning |
//! |---|---|---|
//! | `IRONCLAW_DATA_DIR` | `data` | Root of central DB + per-session files. |
//! | `IRONCLAW_INSTALL_SLUG` | `default` | Label propagated to container runtime for orphan cleanup. |
//! | `IRONCLAW_LOG` | `info` | `tracing-subscriber` env filter. |
//! | `IRONCLAW_ICLAW_SOCKET` | `<data>/iclaw.sock` | Unix socket the `iclaw` client dials. |
//! | `IRONCLAW_DEFAULT_PROVIDER` | unset | Provider injected into new sessions when no per-group override. |
//! | `IRONCLAW_DEFAULT_MODEL` | unset | Default Anthropic model id. |
//! | `IRONCLAW_CHANNELS` | `cli` | Comma-separated list of channels to initialize. |
//! | `IRONCLAW_CHANNELS_CONFIG` | `{}` | JSON object keyed by channel type; per-channel `setup.config`. |
//!
//! The `cli` channel is always implicitly known but is only initialized if it
//! appears in `IRONCLAW_CHANNELS`. Unknown channel names log a warning and
//! are skipped so a typo doesn't fail the boot.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default install slug — used by the runtime to label containers so the
/// orphan-cleanup step picks them up across restarts.
pub const DEFAULT_INSTALL_SLUG: &str = "default";

/// Default `tracing-subscriber` env filter.
pub const DEFAULT_LOG_FILTER: &str = "info";

/// Default data-dir relative to the working directory.
pub const DEFAULT_DATA_DIR: &str = "data";

/// Default channels list.
pub const DEFAULT_CHANNELS: &str = "cli";

/// Channel-by-channel init declaration parsed out of [`HostConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInit {
    /// `ChannelType` string (e.g. `"cli"`, `"telegram"`).
    pub channel_type: String,
    /// Channel-specific configuration blob handed to `ChannelFactory::init`.
    pub config: Value,
}

/// Errors raised when loading or validating [`HostConfig`].
#[derive(Debug, Error)]
pub enum HostConfigError {
    /// `IRONCLAW_CHANNELS_CONFIG` did not parse as JSON.
    #[error("IRONCLAW_CHANNELS_CONFIG is not valid JSON: {0}")]
    BadChannelsConfig(serde_json::Error),
    /// `IRONCLAW_CHANNELS_CONFIG` was not a JSON object.
    #[error("IRONCLAW_CHANNELS_CONFIG must be a JSON object")]
    ChannelsConfigShape,
}

/// Host runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// Where central DB + sessions live.
    pub data_dir: PathBuf,
    /// Label propagated to the container runtime.
    pub install_slug: String,
    /// `tracing-subscriber` env filter.
    pub log_filter: String,
    /// Path to the local Unix socket the `iclaw` client dials.
    pub ncl_socket_path: PathBuf,
    /// Default provider name (or `None` to inherit per-session).
    pub default_provider: Option<String>,
    /// Default model id (or `None`).
    pub default_model: Option<String>,
    /// Default sha-pinned image tag for sessions when the per-group
    /// `container_config.image_tag` is unset. Set by `ironclaw-setup`
    /// after building the image; the host's container manager
    /// requires this to spawn containers on demand.
    pub default_image_tag: Option<String>,
    /// Channels to initialize at boot.
    pub channels: Vec<ChannelInit>,
}

impl HostConfig {
    /// Load configuration from the process environment.
    ///
    /// The caller is responsible for invoking `dotenvy::dotenv().ok()` first
    /// when an `.env` file is desired; this fn intentionally only reads the
    /// current `std::env`.
    pub fn from_env() -> Result<Self, HostConfigError> {
        Self::from_map(&env_to_map())
    }

    /// Pure-function variant of [`Self::from_env`] used by tests. Reads from
    /// an explicit map rather than the process env.
    pub fn from_map(map: &HashMap<String, String>) -> Result<Self, HostConfigError> {
        let data_dir = map
            .get("IRONCLAW_DATA_DIR")
            .map_or_else(|| PathBuf::from(DEFAULT_DATA_DIR), PathBuf::from);
        let install_slug = map
            .get("IRONCLAW_INSTALL_SLUG")
            .cloned()
            .unwrap_or_else(|| DEFAULT_INSTALL_SLUG.to_owned());
        let log_filter = map
            .get("IRONCLAW_LOG")
            .cloned()
            .unwrap_or_else(|| DEFAULT_LOG_FILTER.to_owned());
        let ncl_socket_path = map
            .get("IRONCLAW_ICLAW_SOCKET")
            .map_or_else(|| data_dir.join("iclaw.sock"), PathBuf::from);
        let default_provider = map.get("IRONCLAW_DEFAULT_PROVIDER").cloned();
        let default_model = map.get("IRONCLAW_DEFAULT_MODEL").cloned();
        let default_image_tag = map
            .get("IRONCLAW_DEFAULT_IMAGE_TAG")
            .cloned()
            .filter(|s| !s.is_empty());

        let channels_list = map
            .get("IRONCLAW_CHANNELS")
            .cloned()
            .unwrap_or_else(|| DEFAULT_CHANNELS.to_owned());

        let channels_config_raw = map
            .get("IRONCLAW_CHANNELS_CONFIG")
            .cloned()
            .unwrap_or_else(|| "{}".to_owned());
        let channels_config: Value = serde_json::from_str(&channels_config_raw)
            .map_err(HostConfigError::BadChannelsConfig)?;
        if !channels_config.is_object() {
            return Err(HostConfigError::ChannelsConfigShape);
        }

        let channels = channels_list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|name| {
                let config = channels_config
                    .get(name)
                    .cloned()
                    .unwrap_or(Value::Object(serde_json::Map::new()));
                ChannelInit {
                    channel_type: name.to_owned(),
                    config,
                }
            })
            .collect();

        Ok(Self {
            data_dir,
            install_slug,
            log_filter,
            ncl_socket_path,
            default_provider,
            default_model,
            default_image_tag,
            channels,
        })
    }

    /// Path to the central database file under `data_dir`.
    pub fn central_db_path(&self) -> PathBuf {
        self.data_dir.join("ironclaw.db")
    }

    /// Per-session data root. This is the data directory itself; the
    /// per-session layout (`sessions/<agent_group>/<session>/`) is
    /// appended by [`ironclaw_db::session::SessionPaths::new`] when it
    /// is called with this value as `data_root`.
    ///
    /// Previously this method returned `data_dir/sessions`, which —
    /// combined with `SessionPaths::new`'s own `/sessions/` prefix —
    /// produced the double `data_dir/sessions/sessions/` path. The fix
    /// is to pass `data_dir` directly and let `SessionPaths::new` add
    /// exactly one `sessions/` component.
    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.clone()
    }

    /// Borrow the data root.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        Self::from_map(&HashMap::new()).expect("default config parses")
    }
}

fn env_to_map() -> HashMap<String, String> {
    std::env::vars()
        .filter(|(k, _)| k.starts_with("IRONCLAW_"))
        .collect()
}

/// Try to load an `.env` file. Errors are swallowed (returning `false`)
/// because running without a dotfile is the default case.
///
/// Resolution order:
/// 1. Explicit `path` (passed via `--env-file`).
/// 2. The platform install root's `.env`
///    (`$XDG_DATA_HOME/ironclaw/.env` on Linux,
///    `~/Library/Application Support/ironclaw/.env` on macOS), which
///    is where `ironclaw-setup` writes by default.
/// 3. `.env` in the current working directory (via `dotenvy::dotenv`)
///    as a last-resort supplement. `dotenvy` walks parents looking
///    for the file, so anything above the CWD also counts.
///
/// Vars loaded in step (2) win over step (3) — both `dotenvy` calls
/// only set variables that aren't already in the process env, so the
/// install-root values take precedence by virtue of going first.
pub fn load_dotenv_optional(path: Option<&Path>) -> bool {
    if let Some(p) = path {
        return dotenvy::from_path(p).is_ok();
    }
    let install_loaded = default_install_env_file()
        .is_some_and(|p| p.is_file() && dotenvy::from_path(&p).is_ok());
    let cwd_loaded = dotenvy::dotenv().is_ok();
    install_loaded || cwd_loaded
}

/// Platform-default install-dir `.env` path. Mirrors
/// `ironclaw-setup`'s `default_data_dir_for` so the two binaries
/// agree on where a fresh install lives. Returns `None` when neither
/// `$HOME` nor a platform-specific override is available.
#[must_use]
pub fn default_install_env_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(install_env_file_for(&home, std::env::consts::OS))
}

/// Pure variant of [`default_install_env_file`] used by tests.
#[must_use]
pub fn install_env_file_for(home: &Path, os: &str) -> PathBuf {
    let root = match os {
        "macos" => home
            .join("Library")
            .join("Application Support")
            .join("ironclaw"),
        "linux" => std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|x| !x.as_os_str().is_empty())
            .map_or_else(
                || home.join(".local").join("share").join("ironclaw"),
                |xdg| xdg.join("ironclaw"),
            ),
        _ => home.join(".ironclaw"),
    };
    root.join(".env")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_when_env_empty() {
        let cfg = HostConfig::from_map(&HashMap::new()).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert_eq!(cfg.install_slug, DEFAULT_INSTALL_SLUG);
        assert_eq!(cfg.log_filter, DEFAULT_LOG_FILTER);
        assert_eq!(cfg.ncl_socket_path, PathBuf::from("data/iclaw.sock"));
        assert_eq!(cfg.channels.len(), 1);
        assert_eq!(cfg.channels[0].channel_type, "cli");
        assert!(cfg.default_provider.is_none());
        assert!(cfg.default_model.is_none());
    }

    #[test]
    fn explicit_socket_overrides_default() {
        let cfg =
            HostConfig::from_map(&m(&[("IRONCLAW_ICLAW_SOCKET", "/tmp/some.sock")])).unwrap();
        assert_eq!(cfg.ncl_socket_path, PathBuf::from("/tmp/some.sock"));
    }

    #[test]
    fn data_dir_affects_default_socket_path() {
        let cfg = HostConfig::from_map(&m(&[("IRONCLAW_DATA_DIR", "/srv/ironclaw")])).unwrap();
        assert_eq!(cfg.ncl_socket_path, PathBuf::from("/srv/ironclaw/iclaw.sock"));
        assert_eq!(cfg.central_db_path(), PathBuf::from("/srv/ironclaw/ironclaw.db"));
        // sessions_root() returns data_dir itself; SessionPaths::new then
        // appends sessions/<ag>/<session> to produce the flat layout.
        assert_eq!(
            cfg.sessions_root(),
            PathBuf::from("/srv/ironclaw")
        );
    }

    #[test]
    fn channels_list_parses() {
        let cfg = HostConfig::from_map(&m(&[
            ("IRONCLAW_CHANNELS", "cli, telegram , , slack"),
            (
                "IRONCLAW_CHANNELS_CONFIG",
                "{\"cli\": {\"label\": \"x> \"}, \"telegram\": {\"token\": \"abc\"}}",
            ),
        ]))
        .unwrap();
        assert_eq!(cfg.channels.len(), 3);
        assert_eq!(cfg.channels[0].channel_type, "cli");
        assert_eq!(cfg.channels[0].config["label"], "x> ");
        assert_eq!(cfg.channels[1].channel_type, "telegram");
        assert_eq!(cfg.channels[1].config["token"], "abc");
        // slack listed but has no config blob — defaults to empty object.
        assert_eq!(cfg.channels[2].channel_type, "slack");
        assert!(cfg.channels[2].config.is_object());
    }

    #[test]
    fn empty_channels_list_yields_no_channels() {
        let cfg = HostConfig::from_map(&m(&[("IRONCLAW_CHANNELS", "")])).unwrap();
        assert!(cfg.channels.is_empty());
    }

    #[test]
    fn malformed_channels_config_errors() {
        let err = HostConfig::from_map(&m(&[("IRONCLAW_CHANNELS_CONFIG", "not json")]))
            .unwrap_err();
        assert!(matches!(err, HostConfigError::BadChannelsConfig(_)));
        assert!(err.to_string().contains("IRONCLAW_CHANNELS_CONFIG"));
    }

    #[test]
    fn non_object_channels_config_errors() {
        let err = HostConfig::from_map(&m(&[("IRONCLAW_CHANNELS_CONFIG", "[1,2,3]")]))
            .unwrap_err();
        assert!(matches!(err, HostConfigError::ChannelsConfigShape));
    }

    #[test]
    fn default_provider_and_model_passthrough() {
        let cfg = HostConfig::from_map(&m(&[
            ("IRONCLAW_DEFAULT_PROVIDER", "claude"),
            ("IRONCLAW_DEFAULT_MODEL", "claude-3-5"),
        ]))
        .unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("claude"));
        assert_eq!(cfg.default_model.as_deref(), Some("claude-3-5"));
    }

    #[test]
    fn install_slug_and_log_overrides() {
        let cfg = HostConfig::from_map(&m(&[
            ("IRONCLAW_INSTALL_SLUG", "ci-rig"),
            ("IRONCLAW_LOG", "debug"),
        ]))
        .unwrap();
        assert_eq!(cfg.install_slug, "ci-rig");
        assert_eq!(cfg.log_filter, "debug");
    }

    #[test]
    fn data_dir_accessor() {
        let cfg = HostConfig::default();
        assert_eq!(cfg.data_dir(), Path::new(DEFAULT_DATA_DIR));
    }

    #[test]
    fn channel_init_construction() {
        let c = ChannelInit {
            channel_type: "cli".into(),
            config: serde_json::json!({}),
        };
        assert_eq!(c.channel_type, "cli");
    }

    #[test]
    fn load_dotenv_returns_false_on_missing_path() {
        // Loading from a definitely-missing path returns false (no panic).
        assert!(!load_dotenv_optional(Some(Path::new(
            "/definitely/missing/.env-host-test"
        ))));
    }

    #[test]
    fn from_env_reads_process_env_no_panic() {
        // Just ensure no panic in the no-env happy path. We don't mutate the
        // process env in tests (it's `unsafe` under edition 2024).
        let _ = HostConfig::from_env().unwrap();
    }

    #[test]
    fn install_env_file_for_macos() {
        let p = install_env_file_for(Path::new("/Users/u"), "macos");
        assert_eq!(
            p,
            PathBuf::from("/Users/u/Library/Application Support/ironclaw/.env")
        );
    }

    #[test]
    fn install_env_file_for_linux_fallback() {
        if std::env::var_os("XDG_DATA_HOME").is_some() {
            return;
        }
        let p = install_env_file_for(Path::new("/home/u"), "linux");
        assert_eq!(p, PathBuf::from("/home/u/.local/share/ironclaw/.env"));
    }

    #[test]
    fn install_env_file_for_other_os() {
        let p = install_env_file_for(Path::new("/h"), "freebsd");
        assert_eq!(p, PathBuf::from("/h/.ironclaw/.env"));
    }
}
