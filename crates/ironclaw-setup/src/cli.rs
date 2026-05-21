//! CLI parsing + step driver.
//!
//! `main.rs` is intentionally tiny — it just delegates to [`run_cli`] so the
//! same code can be invoked from integration tests against an in-memory
//! argv.

use crate::config::SetupConfig;
use crate::migrator::migrate_from;
use crate::prompt::{EnvBacked, Interactive, Prompt};
use crate::state::SetupState;
use crate::steps::{all_steps, Step, StepError};
use crate::units::{generate, UnitContext, UnitKind};
use clap::Parser;
use std::path::PathBuf;

/// Parsed CLI arguments.
#[derive(Debug, Clone, Parser, Default)]
#[command(
    name = "ironclaw-setup",
    about = "Interactive first-time setup for an ironclaw host",
    version,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Override the data directory.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Run without interactive prompts; read answers from env vars.
    #[arg(long)]
    pub headless: bool,
    /// Copy an existing data directory before running setup.
    #[arg(long)]
    pub migrate_from: Option<PathBuf>,
    /// Alias for `--headless` retained for ergonomics.
    #[arg(long)]
    pub non_interactive: bool,
    /// Skip a step by name (may be passed multiple times).
    #[arg(long = "skip-step")]
    pub skip_step: Vec<String>,
    /// Print the canonical list of steps and exit.
    #[arg(long)]
    pub list_steps: bool,
    /// Generate a service unit and exit (`systemd` | `launchd`).
    #[arg(long, value_name = "KIND")]
    pub generate_unit: Option<String>,
    /// Output path for `--generate-unit`. Defaults to stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

impl Cli {
    /// Whether the run should be driven without interactive prompts.
    #[must_use]
    pub fn is_headless(&self) -> bool {
        self.headless || self.non_interactive
    }
}

/// Top-level driver.
///
/// Implements `--list-steps`, `--generate-unit`, `--migrate-from`, and the
/// normal setup loop.
pub fn run_cli(cli: Cli) -> Result<i32, StepError> {
    if cli.list_steps {
        for s in all_steps() {
            println!("{}\t{}", s.name(), s.description());
        }
        return Ok(0);
    }

    if let Some(kind_str) = cli.generate_unit.as_deref() {
        let kind = UnitKind::parse(kind_str).map_err(StepError::Other)?;
        let cfg = SetupConfig::default();
        let ctx = UnitContext::new(
            PathBuf::from("/usr/local/bin/ironclaw"),
            cli.data_dir.unwrap_or_else(|| PathBuf::from(".")),
            PathBuf::from(".env"),
        );
        let _ = cfg;
        let body = generate(kind, &ctx, None);
        if let Some(out) = cli.out {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&out, body)?;
            println!("wrote {}", out.display());
        } else {
            print!("{body}");
        }
        return Ok(0);
    }

    let mut config = SetupConfig::default();
    if let Some(dir) = cli.data_dir.clone() {
        config.data_dir = dir;
    }

    if let Some(src) = cli.migrate_from.as_deref() {
        let dest = if config.data_dir.as_os_str().is_empty() {
            crate::steps::data_dir::default_data_dir().ok_or_else(|| {
                StepError::Other("no destination data dir for --migrate-from".to_string())
            })?
        } else {
            config.data_dir.clone()
        };
        let outcome =
            migrate_from(src, &dest).map_err(|e| StepError::Other(format!("migrate: {e}")))?;
        config.data_dir = dest;
        config.central_db_path = outcome.central_db_path;
        println!(
            "migrated from {} (copied_db={})",
            src.display(),
            outcome.copied_db
        );
    }

    let prompt: Box<dyn Prompt> = if cli.is_headless() {
        Box::new(EnvBacked::from_process_env())
    } else {
        Box::new(Interactive::new())
    };
    let mut state = if config.data_dir.as_os_str().is_empty() {
        SetupState::new()
    } else {
        SetupState::load(&config.data_dir)
            .map_err(|e| StepError::Other(format!("load state: {e}")))?
    };
    if state.config != SetupConfig::default() {
        config = state.config.clone();
    }
    let steps = all_steps();
    run_steps(&steps, &cli.skip_step, &mut config, prompt.as_ref(), &mut state)?;
    state.config = config.clone();
    if !config.data_dir.as_os_str().is_empty() {
        state
            .save(&config.data_dir)
            .map_err(|e| StepError::Other(format!("save state: {e}")))?;
    }
    Ok(0)
}

/// Run each step in `steps`, honoring `--skip-step` and writing state.
pub fn run_steps(
    steps: &[Box<dyn Step>],
    skip: &[String],
    config: &mut SetupConfig,
    prompt: &dyn Prompt,
    state: &mut SetupState,
) -> Result<(), StepError> {
    for step in steps {
        if skip.iter().any(|s| s == step.name()) {
            if !step.is_skippable() {
                return Err(StepError::Other(format!(
                    "step `{}` cannot be skipped",
                    step.name()
                )));
            }
            println!("[skip] {}", step.name());
            continue;
        }
        println!("[step] {}: {}", step.name(), step.description());
        let result = step.run(config, prompt, state)?;
        for line in &result.messages {
            println!("  {line}");
        }
        if result.config_changed {
            state.config = config.clone();
            state.mark_completed(step.name());
            if !config.data_dir.as_os_str().is_empty() {
                state
                    .save(&config.data_dir)
                    .map_err(|e| StepError::Other(format!("save state: {e}")))?;
            }
        }
    }
    Ok(())
}

/// Parse argv-style args and run.
pub fn run_from_args<I, T>(args: I) -> Result<i32, StepError>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = Cli::try_parse_from(args).map_err(|e| StepError::Other(e.to_string()))?;
    run_cli(cli)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    fn passthrough_steps() -> Vec<Box<dyn Step>> {
        struct A;
        impl Step for A {
            fn name(&self) -> &'static str {
                "a"
            }
            fn description(&self) -> &'static str {
                "first"
            }
            fn run(
                &self,
                _cfg: &mut SetupConfig,
                _p: &dyn Prompt,
                _s: &mut SetupState,
            ) -> Result<crate::steps::StepResult, StepError> {
                Ok(crate::steps::StepResult::ok("a-ran"))
            }
        }
        struct B;
        impl Step for B {
            fn name(&self) -> &'static str {
                "b"
            }
            fn description(&self) -> &'static str {
                "second"
            }
            fn is_skippable(&self) -> bool {
                false
            }
            fn run(
                &self,
                _cfg: &mut SetupConfig,
                _p: &dyn Prompt,
                _s: &mut SetupState,
            ) -> Result<crate::steps::StepResult, StepError> {
                Ok(crate::steps::StepResult::ok("b-ran"))
            }
        }
        let v: Vec<Box<dyn Step>> = vec![Box::new(A), Box::new(B)];
        v
    }

    #[test]
    fn cli_default_is_interactive() {
        let cli = Cli::default();
        assert!(!cli.is_headless());
    }

    #[test]
    fn cli_headless_flag() {
        let cli = Cli {
            headless: true,
            ..Cli::default()
        };
        assert!(cli.is_headless());
    }

    #[test]
    fn cli_non_interactive_flag_is_headless() {
        let cli = Cli {
            non_interactive: true,
            ..Cli::default()
        };
        assert!(cli.is_headless());
    }

    #[test]
    fn cli_parses_list_steps() {
        let cli = Cli::try_parse_from(["ironclaw-setup", "--list-steps"]).unwrap();
        assert!(cli.list_steps);
    }

    #[test]
    fn cli_parses_skip_step_multiple() {
        let cli = Cli::try_parse_from([
            "ironclaw-setup",
            "--skip-step",
            "image",
            "--skip-step",
            "onecli",
        ])
        .unwrap();
        assert_eq!(cli.skip_step, vec!["image".to_string(), "onecli".to_string()]);
    }

    #[test]
    fn cli_parses_generate_unit_and_out() {
        let cli = Cli::try_parse_from([
            "ironclaw-setup",
            "--generate-unit",
            "systemd",
            "--out",
            "/tmp/foo.service",
        ])
        .unwrap();
        assert_eq!(cli.generate_unit.as_deref(), Some("systemd"));
        assert_eq!(cli.out, Some(PathBuf::from("/tmp/foo.service")));
    }

    #[test]
    fn run_steps_executes_each_in_order() {
        let dir = tempdir().unwrap();
        let steps = passthrough_steps();
        let prompt = Scripted::new();
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        run_steps(&steps, &[], &mut cfg, &prompt, &mut state).unwrap();
        assert!(state.is_completed("a"));
        assert!(state.is_completed("b"));
    }

    #[test]
    fn run_steps_skips_named() {
        let steps = passthrough_steps();
        let prompt = Scripted::new();
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        run_steps(&steps, &["a".to_string()], &mut cfg, &prompt, &mut state).unwrap();
        assert!(!state.is_completed("a"));
        assert!(state.is_completed("b"));
    }

    #[test]
    fn run_steps_rejects_skipping_non_skippable() {
        let steps = passthrough_steps();
        let prompt = Scripted::new();
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let err =
            run_steps(&steps, &["b".to_string()], &mut cfg, &prompt, &mut state).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    #[test]
    fn run_from_args_list_steps() {
        let code = run_from_args(["ironclaw-setup", "--list-steps"]).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn run_from_args_generate_unit_stdout() {
        let code = run_from_args(["ironclaw-setup", "--generate-unit", "launchd"]).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn run_from_args_generate_unit_with_out() {
        let dir = tempdir().unwrap();
        let out = dir.path().join("x/sample.service");
        let code = run_from_args([
            "ironclaw-setup",
            "--generate-unit",
            "systemd",
            "--out",
            &out.to_string_lossy(),
        ])
        .unwrap();
        assert_eq!(code, 0);
        assert!(out.exists());
    }

    #[test]
    fn run_from_args_unknown_unit_errors() {
        let err = run_from_args(["ironclaw-setup", "--generate-unit", "upstart"]).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    #[test]
    fn step_by_name_is_used_via_skip() {
        // Sanity: known names from the canonical registry are addressable.
        assert!(crate::steps::step_by_name("env_check").is_some());
        assert!(crate::steps::step_by_name("verify").is_some());
    }

    #[test]
    fn run_from_args_bad_flag_errors() {
        let err = run_from_args(["ironclaw-setup", "--no-such-flag"]).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }
}
