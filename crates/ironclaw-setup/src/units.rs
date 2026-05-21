//! Systemd unit + launchd plist generators.
//!
//! Templates are optionally loaded from disk (the repo root ships
//! `systemd/` and `launchd/` directories that contain reference templates);
//! when a template is unavailable the inline default in this module is used
//! so the binary remains self-contained.

use std::path::{Path, PathBuf};

/// Inputs needed to render a unit / plist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitContext {
    /// Absolute path to the `ironclaw` binary the unit should exec.
    pub exec_path: PathBuf,
    /// Data directory passed to the binary.
    pub data_dir: PathBuf,
    /// Path to the `.env` file the unit should source.
    pub env_file: PathBuf,
}

impl UnitContext {
    /// Convenience constructor.
    pub fn new(
        exec_path: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        env_file: impl Into<PathBuf>,
    ) -> Self {
        Self {
            exec_path: exec_path.into(),
            data_dir: data_dir.into(),
            env_file: env_file.into(),
        }
    }
}

/// Which unit flavor to generate.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum UnitKind {
    /// Systemd user unit (Linux).
    Systemd,
    /// launchd plist (macOS).
    Launchd,
}

impl UnitKind {
    /// Stable token used in CLI args and filenames.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
        }
    }

    /// Parse a kind name from `--generate-unit <kind>`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "systemd" => Ok(Self::Systemd),
            "launchd" => Ok(Self::Launchd),
            other => Err(format!(
                "unknown unit kind `{other}` (expected `systemd` or `launchd`)"
            )),
        }
    }

    /// Default filename for this kind of unit.
    #[must_use]
    pub fn default_filename(self) -> &'static str {
        match self {
            Self::Systemd => "ironclaw.service",
            Self::Launchd => "com.ironclaw.host.plist",
        }
    }
}

/// Render a systemd user unit.
#[must_use]
pub fn render_systemd(ctx: &UnitContext) -> String {
    let exec = ctx.exec_path.display();
    let data_dir = ctx.data_dir.display();
    let env_file = ctx.env_file.display();
    format!(
        "[Unit]\n\
         Description=ironclaw host\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         EnvironmentFile={env_file}\n\
         Environment=IRONCLAW_DATA_DIR={data_dir}\n\
         ExecStart={exec} run --data-dir {data_dir}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Render a launchd user plist.
#[must_use]
pub fn render_launchd(ctx: &UnitContext) -> String {
    let exec = ctx.exec_path.display();
    let data_dir = ctx.data_dir.display();
    let env_file = ctx.env_file.display();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n  \
           <key>Label</key>\n  <string>com.ironclaw.host</string>\n  \
           <key>ProgramArguments</key>\n  <array>\n    \
             <string>{exec}</string>\n    \
             <string>run</string>\n    \
             <string>--data-dir</string>\n    \
             <string>{data_dir}</string>\n  \
           </array>\n  \
           <key>EnvironmentVariables</key>\n  <dict>\n    \
             <key>IRONCLAW_DATA_DIR</key>\n    <string>{data_dir}</string>\n  \
           </dict>\n  \
           <key>RunAtLoad</key>\n  <true/>\n  \
           <key>KeepAlive</key>\n  <true/>\n  \
           <key>StandardOutPath</key>\n  <string>{data_dir}/logs/ironclaw.out.log</string>\n  \
           <key>StandardErrorPath</key>\n  <string>{data_dir}/logs/ironclaw.err.log</string>\n  \
           <key>EnvFile</key>\n  <string>{env_file}</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// Apply a template by replacing `${EXEC}`, `${DATA_DIR}`, and `${ENV_FILE}`
/// placeholders. The repo's `systemd/` and `launchd/` directories ship
/// reference templates using exactly this syntax.
#[must_use]
pub fn apply_template(template: &str, ctx: &UnitContext) -> String {
    template
        .replace("${EXEC}", &ctx.exec_path.display().to_string())
        .replace("${DATA_DIR}", &ctx.data_dir.display().to_string())
        .replace("${ENV_FILE}", &ctx.env_file.display().to_string())
}

/// Generate either a systemd unit or a launchd plist.
///
/// Loads a template from the matching subdirectory of `template_root` when
/// available, else falls back to the inline renderer.
#[must_use]
pub fn generate(
    kind: UnitKind,
    ctx: &UnitContext,
    template_root: Option<&Path>,
) -> String {
    let template_path = template_root.map(|root| match kind {
        UnitKind::Systemd => root.join("systemd").join("ironclaw.service"),
        UnitKind::Launchd => root.join("launchd").join("com.ironclaw.host.plist"),
    });
    if let Some(path) = template_path {
        if let Ok(template) = std::fs::read_to_string(&path) {
            if !template.trim().is_empty() {
                return apply_template(&template, ctx);
            }
        }
    }
    match kind {
        UnitKind::Systemd => render_systemd(ctx),
        UnitKind::Launchd => render_launchd(ctx),
    }
}

/// Default installation path for a given unit kind, under the user's home.
///
/// Returns `None` when `$HOME` is not set.
#[must_use]
pub fn default_install_path(kind: UnitKind, home: &Path) -> PathBuf {
    match kind {
        UnitKind::Systemd => home
            .join(".config")
            .join("systemd")
            .join("user")
            .join(kind.default_filename()),
        UnitKind::Launchd => home
            .join("Library")
            .join("LaunchAgents")
            .join(kind.default_filename()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> UnitContext {
        UnitContext::new("/usr/local/bin/ironclaw", "/srv/ironclaw", "/srv/ironclaw/.env")
    }

    #[test]
    fn unit_kind_as_str() {
        assert_eq!(UnitKind::Systemd.as_str(), "systemd");
        assert_eq!(UnitKind::Launchd.as_str(), "launchd");
    }

    #[test]
    fn unit_kind_default_filename() {
        assert_eq!(UnitKind::Systemd.default_filename(), "ironclaw.service");
        assert_eq!(
            UnitKind::Launchd.default_filename(),
            "com.ironclaw.host.plist"
        );
    }

    #[test]
    fn unit_kind_parse_systemd() {
        assert_eq!(UnitKind::parse("systemd").unwrap(), UnitKind::Systemd);
    }

    #[test]
    fn unit_kind_parse_launchd() {
        assert_eq!(UnitKind::parse("launchd").unwrap(), UnitKind::Launchd);
    }

    #[test]
    fn unit_kind_parse_unknown_errors() {
        let err = UnitKind::parse("upstart").unwrap_err();
        assert!(err.contains("upstart"));
    }

    #[test]
    fn render_systemd_snapshot() {
        let out = render_systemd(&ctx());
        let expected = "[Unit]\n\
            Description=ironclaw host\n\
            After=network-online.target\n\
            Wants=network-online.target\n\
            \n\
            [Service]\n\
            Type=simple\n\
            EnvironmentFile=/srv/ironclaw/.env\n\
            Environment=IRONCLAW_DATA_DIR=/srv/ironclaw\n\
            ExecStart=/usr/local/bin/ironclaw run --data-dir /srv/ironclaw\n\
            Restart=on-failure\n\
            RestartSec=5\n\
            \n\
            [Install]\n\
            WantedBy=default.target\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_launchd_snapshot() {
        let out = render_launchd(&ctx());
        assert!(out.starts_with("<?xml"));
        assert!(out.contains("<string>com.ironclaw.host</string>"));
        assert!(out.contains("<string>/usr/local/bin/ironclaw</string>"));
        assert!(out.contains("<string>--data-dir</string>"));
        assert!(out.contains("<string>/srv/ironclaw</string>"));
        assert!(out.contains("/srv/ironclaw/logs/ironclaw.out.log"));
        assert!(out.contains("/srv/ironclaw/logs/ironclaw.err.log"));
        assert!(out.contains("/srv/ironclaw/.env"));
        assert!(out.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn apply_template_substitutes_all_placeholders() {
        let tmpl =
            "EXEC=${EXEC}\nDATA=${DATA_DIR}\nENV=${ENV_FILE}\n";
        let out = apply_template(tmpl, &ctx());
        assert_eq!(
            out,
            "EXEC=/usr/local/bin/ironclaw\nDATA=/srv/ironclaw\nENV=/srv/ironclaw/.env\n"
        );
    }

    #[test]
    fn generate_uses_template_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let sysd = dir.path().join("systemd");
        std::fs::create_dir_all(&sysd).unwrap();
        std::fs::write(
            sysd.join("ironclaw.service"),
            "TEMPLATE EXEC=${EXEC} DATA=${DATA_DIR} ENV=${ENV_FILE}\n",
        )
        .unwrap();
        let out = generate(UnitKind::Systemd, &ctx(), Some(dir.path()));
        assert!(out.starts_with("TEMPLATE EXEC=/usr/local/bin/ironclaw"));
    }

    #[test]
    fn generate_falls_back_when_template_missing() {
        let dir = tempfile::tempdir().unwrap();
        let out = generate(UnitKind::Systemd, &ctx(), Some(dir.path()));
        assert!(out.contains("[Service]"));
    }

    #[test]
    fn generate_falls_back_when_template_empty() {
        let dir = tempfile::tempdir().unwrap();
        let sysd = dir.path().join("systemd");
        std::fs::create_dir_all(&sysd).unwrap();
        std::fs::write(sysd.join("ironclaw.service"), "   \n").unwrap();
        let out = generate(UnitKind::Systemd, &ctx(), Some(dir.path()));
        assert!(out.contains("[Service]"));
    }

    #[test]
    fn generate_launchd_no_template_root() {
        let out = generate(UnitKind::Launchd, &ctx(), None);
        assert!(out.starts_with("<?xml"));
    }

    #[test]
    fn generate_launchd_uses_template_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let lnchd = dir.path().join("launchd");
        std::fs::create_dir_all(&lnchd).unwrap();
        std::fs::write(
            lnchd.join("com.ironclaw.host.plist"),
            "PLIST=${EXEC} DATA=${DATA_DIR}\n",
        )
        .unwrap();
        let out = generate(UnitKind::Launchd, &ctx(), Some(dir.path()));
        assert!(out.starts_with("PLIST=/usr/local/bin/ironclaw"));
    }

    #[test]
    fn default_install_path_systemd_under_config() {
        let p = default_install_path(UnitKind::Systemd, Path::new("/home/u"));
        assert_eq!(
            p,
            PathBuf::from("/home/u/.config/systemd/user/ironclaw.service")
        );
    }

    #[test]
    fn default_install_path_launchd_under_library() {
        let p = default_install_path(UnitKind::Launchd, Path::new("/Users/u"));
        assert_eq!(
            p,
            PathBuf::from("/Users/u/Library/LaunchAgents/com.ironclaw.host.plist")
        );
    }

    #[test]
    fn unit_context_new_stores_inputs() {
        let c = UnitContext::new("/x", "/d", "/e");
        assert_eq!(c.exec_path, PathBuf::from("/x"));
        assert_eq!(c.data_dir, PathBuf::from("/d"));
        assert_eq!(c.env_file, PathBuf::from("/e"));
    }
}
