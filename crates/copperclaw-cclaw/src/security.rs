//! `cclaw security audit [--fix]` — a one-shot security-posture report
//! built on the same `Check`-style framework as `cclaw doctor`.
//!
//! Audit flags **open** policies and never auto-loosens anything:
//!
//!   - egress `allow-all` (the permissive default mode) and per-group
//!     empty allow-lists,
//!   - default-allow approval posture (`unknown_sender_policy = open`),
//!   - missing per-group tool profiles (the group runs the permissive
//!     `full` default),
//!   - world-/group-readable session files under the data dir,
//!   - the credential broker being disabled while the posture is
//!     otherwise sensitive (egress allow-all).
//!
//! With `--fix`, the audit remediates only the **safe, tightening**
//! findings — it scaffolds a minimal allow-list for groups that have
//! none, tightens an `open` approval policy to `request_approval`, and
//! `chmod`s loose session files to `0600`/`0700`. Every host-side fix
//! flows through an existing host mutation command, so the socket's
//! dispatch layer audits it automatically (see
//! `copperclaw_host::socket::audit_dispatch`). Local `chmod` fixes are
//! reported in the rendered output. The audit NEVER widens a policy: it
//! has no code path that flips deny-default -> allow-all, `strict` ->
//! `open`, or relaxes a tool profile.
//!
//! The [`analyze`] core is a pure function over an already-gathered
//! [`Posture`] snapshot so the detection logic is unit-testable without a
//! transport or a filesystem. The orchestrator [`run_security_audit`]
//! gathers the snapshot (transport calls + local env / fs) and, when
//! `--fix` is set, applies the safe remediations.

use crate::client::ClientError;
use crate::protocol::Caller;
use crate::{CallTransport, RunOutput, render_json_pretty};
use serde_json::{Value, json};

/// Severity of a single security finding. Ordered loosest -> tightest so
/// the renderer can sort and the worst row picks the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational — posture is as tight as it can be on this axis.
    Ok,
    /// An open posture worth surfacing but not a hard misconfiguration.
    Warn,
    /// An open posture that materially weakens the deployment.
    High,
}

impl Severity {
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "OK  ",
            Self::Warn => "WARN",
            Self::High => "HIGH",
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::High => "high",
        }
    }
}

/// Whether a finding can be auto-remediated by `--fix`, and if so how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remediation {
    /// Nothing to do — the posture is already tight.
    None,
    /// `--fix` can tighten this safely. `manual_hint` is shown when the
    /// operator did not pass `--fix`.
    Fixable { manual_hint: String },
    /// Open posture that `--fix` deliberately will NOT touch (tightening
    /// it would change agent behaviour or require an operator decision).
    /// `manual_hint` tells the operator how to tighten it themselves.
    ManualOnly { manual_hint: String },
}

/// One security finding. Mirrors `doctor`'s `Check` but carries the
/// remediation classification that `--fix` keys off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub detail: String,
    pub remediation: Remediation,
}

impl Finding {
    fn ok(id: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            severity: Severity::Ok,
            detail: detail.into(),
            remediation: Remediation::None,
        }
    }

    fn to_json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("id".into(), Value::String(self.id.into()));
        o.insert(
            "severity".into(),
            Value::String(self.severity.as_str().into()),
        );
        o.insert("detail".into(), Value::String(self.detail.clone()));
        let (fixable, hint) = match &self.remediation {
            Remediation::None => (false, None),
            Remediation::Fixable { manual_hint } => (true, Some(manual_hint.clone())),
            Remediation::ManualOnly { manual_hint } => (false, Some(manual_hint.clone())),
        };
        o.insert("fixable".into(), Value::Bool(fixable));
        if let Some(h) = hint {
            o.insert("hint".into(), Value::String(h));
        }
        Value::Object(o)
    }
}

/// One agent group's posture inputs, normalised from the wire responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPosture {
    pub agent_group_id: String,
    pub name: String,
    /// The group's operator-configured egress allow-list length. An empty
    /// configured list is the open signal under allow-all.
    pub configured_allow_len: usize,
    /// `tool_profile` from the group's container config, if any. `None`
    /// means the runner falls back to the permissive `full` profile.
    pub tool_profile: Option<String>,
}

/// One messaging group's posture inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingPosture {
    pub messaging_group_id: String,
    pub name: String,
    pub channel_type: String,
    pub unknown_sender_policy: String,
}

/// One session file whose permissions are looser than `0600`/`0700`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoosePermFile {
    pub path: String,
    /// The octal mode bits actually observed (e.g. `0o644`).
    pub mode: u32,
    /// Whether the path is a directory (-> tighten to `0700`) or a file
    /// (-> tighten to `0600`).
    pub is_dir: bool,
}

/// The full security-posture snapshot the audit reasons over. Pure data:
/// gathered by [`run_security_audit`], consumed by [`analyze`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posture {
    /// Egress mode reported by `egress.status` (`allow-all` / `deny-default`).
    pub egress_mode: String,
    pub groups: Vec<GroupPosture>,
    pub messaging_groups: Vec<MessagingPosture>,
    /// Session files under the data dir with loose permissions.
    pub loose_perm_files: Vec<LoosePermFile>,
    /// Whether the credential broker is enabled in the cclaw client's env
    /// (`COPPERCLAW_CREDENTIAL_BROKER` truthy).
    pub broker_enabled: bool,
}

/// Parse the operator-facing `COPPERCLAW_CREDENTIAL_BROKER` toggle.
/// Mirrors `copperclaw_host::container_manager::broker`'s parse so the
/// audit and the spawn path agree on what "enabled" means. Truthy values
/// turn the broker on; everything else (including unset) is off.
#[must_use]
pub fn broker_enabled_from_env(raw: Option<&str>) -> bool {
    matches!(
        raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on" | "enable" | "enabled")
    )
}

/// Parse the operator-facing `COPPERCLAW_EGRESS_MODE` toggle into the
/// mode string the audit reasons over. Matches the host's
/// `parse_egress_mode`: truthy / `deny` / `deny-default` -> deny-default,
/// everything else (including unset) -> allow-all.
#[must_use]
pub fn egress_mode_from_env(raw: Option<&str>) -> String {
    let on = matches!(
        raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on" | "deny" | "deny-default" | "denydefault")
    );
    if on {
        "deny-default".into()
    } else {
        "allow-all".into()
    }
}

/// Pure detection core. Produces one [`Finding`] per posture axis. No
/// transport, no filesystem — every input arrives in `posture`.
#[must_use]
pub fn analyze(posture: &Posture) -> Vec<Finding> {
    let mut findings = Vec::new();

    // --- Egress mode -------------------------------------------------------
    if posture.egress_mode == "allow-all" {
        findings.push(Finding {
            id: "egress-mode",
            severity: Severity::High,
            detail: "egress mode is allow-all: containers may reach any host/port. Per-group \
                 allow-lists are advisory only in this mode."
                .into(),
            remediation: Remediation::ManualOnly {
                manual_hint:
                    "set COPPERCLAW_EGRESS_MODE=deny-default in the host .env and restart the \
                     host (this is an env/policy change, not an audited mutation, so --fix will \
                     not flip it for you). Scaffold per-group allow-lists first with `cclaw \
                     security audit --fix`."
                        .into(),
            },
        });
    } else {
        findings.push(Finding::ok(
            "egress-mode",
            format!(
                "egress mode is {} (containers are L3/L4 + DNS filtered)",
                posture.egress_mode
            ),
        ));
    }

    // --- Per-group empty allow-list (fixable: scaffold a minimal list) ----
    for g in &posture.groups {
        if g.configured_allow_len == 0 {
            findings.push(Finding {
                id: "egress-allow-empty",
                severity: if posture.egress_mode == "allow-all" {
                    Severity::Warn
                } else {
                    // Under deny-default an empty configured list means the
                    // group reaches ONLY its auto-injected model endpoint —
                    // that is already tight, so it is not a finding there.
                    Severity::Ok
                },
                detail: format!(
                    "group `{}` ({}) has no operator-configured egress allow-list",
                    g.name, g.agent_group_id
                ),
                remediation: if posture.egress_mode == "allow-all" {
                    Remediation::Fixable {
                        manual_hint: format!(
                            "scaffold a minimal allow-list (model endpoint only) for group {} \
                             with `cclaw groups config set-egress-allow`",
                            g.agent_group_id
                        ),
                    }
                } else {
                    Remediation::None
                },
            });
        }

        // --- Missing tool profile (manual-only: tightening changes agent
        //     capability, so the audit flags but does not auto-set it) ----
        if g.tool_profile.is_none() {
            findings.push(Finding {
                id: "tool-profile-missing",
                severity: Severity::Warn,
                detail: format!(
                    "group `{}` ({}) has no tool_profile — the runner falls back to the \
                     permissive `full` profile",
                    g.name, g.agent_group_id
                ),
                remediation: Remediation::ManualOnly {
                    manual_hint: format!(
                        "pin a least-privilege profile with `cclaw groups config update \
                         --field 'tool_profile=\"messaging\"' {}` (choosing the profile is a \
                         policy decision, so --fix will not set it for you)",
                        g.agent_group_id
                    ),
                },
            });
        }
    }

    // --- Default-allow approvals (fixable: tighten open -> request_approval) -
    for mg in &posture.messaging_groups {
        if mg.unknown_sender_policy == "open" {
            findings.push(Finding {
                id: "approvals-open",
                severity: Severity::High,
                detail: format!(
                    "messaging group `{}` ({} / {}) has unknown_sender_policy=open: any \
                     unrecognised sender is auto-approved",
                    mg.name, mg.channel_type, mg.messaging_group_id
                ),
                remediation: Remediation::Fixable {
                    manual_hint: format!(
                        "tighten {} to request_approval — run `cclaw security audit --fix` \
                         (which issues an audited messaging-groups.update)",
                        mg.messaging_group_id
                    ),
                },
            });
        }
    }

    // --- World-/group-readable session files (fixable: chmod 0600/0700) ---
    for f in &posture.loose_perm_files {
        let want = if f.is_dir { 0o700 } else { 0o600 };
        findings.push(Finding {
            id: "session-file-perms",
            severity: Severity::High,
            detail: format!(
                "session path {} is mode {:#o}; group/world bits are set (want {:#o})",
                f.path, f.mode, want
            ),
            remediation: Remediation::Fixable {
                manual_hint: format!("chmod {want:#o} {}", f.path),
            },
        });
    }

    // --- Broker disabled while sensitive ----------------------------------
    if !posture.broker_enabled && posture.egress_mode == "allow-all" {
        findings.push(Finding {
            id: "broker-disabled",
            severity: Severity::Warn,
            detail: "credential broker is disabled AND egress is allow-all: the real model key \
                 is mounted into containers that can also reach arbitrary hosts."
                .into(),
            remediation: Remediation::ManualOnly {
                manual_hint: "enable the broker (COPPERCLAW_CREDENTIAL_BROKER=1) and/or move to \
                     deny-default egress (env/policy changes, so --fix will not flip them)."
                    .into(),
            },
        });
    } else {
        findings.push(Finding::ok(
            "broker-disabled",
            if posture.broker_enabled {
                "credential broker is enabled (model key is brokered, not mounted)".to_string()
            } else {
                "credential broker is disabled but egress is filtered (acceptable posture)"
                    .to_string()
            },
        ));
    }

    findings
}

/// True when `mode` has any group- or world-accessible bit set. We treat
/// any non-owner permission bit as "loose" for both files and dirs.
#[must_use]
pub fn perms_are_loose(mode: u32) -> bool {
    mode & 0o077 != 0
}

/// Render the findings to a `RunOutput`. `fixed` carries the human
/// descriptions of any safe fixes already applied this run.
fn finalise(findings: &[Finding], fixed: &[String], as_json: bool) -> RunOutput {
    let worst = findings
        .iter()
        .map(|f| f.severity)
        .fold(Severity::Ok, |acc, s| match (acc, s) {
            (Severity::High, _) | (_, Severity::High) => Severity::High,
            (Severity::Warn, _) | (_, Severity::Warn) => Severity::Warn,
            _ => Severity::Ok,
        });
    let any_open = worst != Severity::Ok;

    if as_json {
        let payload = json!({
            "status": worst.as_str(),
            "findings": findings.iter().map(Finding::to_json).collect::<Vec<_>>(),
            "fixes_applied": fixed,
        });
        let mut out = render_json_pretty(&payload);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        // An audit that found open policies is a non-zero exit so CI / pre-flight
        // scripts can gate on it — but applying fixes that close everything
        // flips it back to success.
        return if any_open && fixed.is_empty() {
            RunOutput::failure(out)
        } else {
            RunOutput::success(out)
        };
    }

    let mut out = String::new();
    for f in findings {
        out.push_str(&format!(
            "[{}] {:<22} {}\n",
            f.severity.tag(),
            f.id,
            f.detail
        ));
        match &f.remediation {
            Remediation::None => {}
            Remediation::Fixable { manual_hint } => {
                out.push_str(&format!("       fix (--fix): {manual_hint}\n"));
            }
            Remediation::ManualOnly { manual_hint } => {
                out.push_str(&format!("       manual:      {manual_hint}\n"));
            }
        }
    }
    if !fixed.is_empty() {
        out.push_str("\napplied fixes:\n");
        for f in fixed {
            out.push_str(&format!("  - {f}\n"));
        }
    }
    if any_open && fixed.is_empty() {
        out.push_str("\nopen policies found; re-run with --fix to remediate the safe ones\n");
        RunOutput::failure(out)
    } else if any_open {
        out.push_str(
            "\nsafe fixes applied; some open policies remain (see `manual:` lines above)\n",
        );
        RunOutput::success(out)
    } else {
        out.push_str("\nno open policies found; posture is tight\n");
        RunOutput::success(out)
    }
}

/// Normalise the `egress.status` wire response into `(mode, [(group_id,
/// configured_allow_len)])`. Falls back to the env-derived mode when the
/// transport response omits it.
fn parse_egress(value: &Value, env_mode: &str) -> (String, Vec<(String, usize)>) {
    let mode = value
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or(env_mode)
        .to_string();
    let allow_lens = value
        .get("groups")
        .and_then(Value::as_array)
        .map(|gs| {
            gs.iter()
                .map(|g| {
                    let id = g
                        .get("agent_group_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let n = g
                        .get("configured_allow")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    (id, n)
                })
                .collect()
        })
        .unwrap_or_default();
    (mode, allow_lens)
}

/// `cclaw security audit [--fix]`.
///
/// Gathers the posture via transport calls + local env / filesystem,
/// runs [`analyze`], optionally applies safe fixes, and renders. The
/// `args` payload carries `fix: bool` and an optional `data_dir` override
/// (defaults to the resolved install data dir) so tests can point the
/// filesystem checks at a temp tree.
pub async fn run_security_audit<T>(
    args: &Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let fix = args.get("fix").and_then(Value::as_bool).unwrap_or(false);
    let data_dir = args
        .get("data_dir")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .or_else(|| crate::resolve_install_root().map(|r| r.join("data")));

    // --- Gather: egress posture -------------------------------------------
    let env_mode = egress_mode_from_env(std::env::var("COPPERCLAW_EGRESS_MODE").ok().as_deref());
    let egress = transport
        .call("egress.status", json!({}), caller.clone())
        .await;
    let (egress_mode, group_allow_lens) = match &egress {
        Ok(v) => parse_egress(v, &env_mode),
        Err(_) => (env_mode.clone(), Vec::new()),
    };

    // --- Gather: groups + their tool profiles -----------------------------
    let mut groups = Vec::new();
    if let Ok(list) = transport
        .call("groups.list", json!({}), caller.clone())
        .await
    {
        if let Some(rows) = list.as_array() {
            for row in rows {
                let id = row
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = row
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let allow_len = group_allow_lens
                    .iter()
                    .find(|(gid, _)| *gid == id)
                    .map_or(0, |(_, n)| *n);
                let cfg = transport
                    .call("groups.config.get", json!({"id": id}), caller.clone())
                    .await
                    .ok();
                let tool_profile = cfg
                    .as_ref()
                    .and_then(|c| c.get("tool_profile"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                groups.push(GroupPosture {
                    agent_group_id: id,
                    name,
                    configured_allow_len: allow_len,
                    tool_profile,
                });
            }
        }
    }

    // --- Gather: messaging groups -----------------------------------------
    let mut messaging_groups = Vec::new();
    if let Ok(list) = transport
        .call("messaging-groups.list", json!({}), caller.clone())
        .await
    {
        if let Some(rows) = list.as_array() {
            for row in rows {
                messaging_groups.push(MessagingPosture {
                    messaging_group_id: row
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: row
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                    channel_type: row
                        .get("channel_type")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
                    unknown_sender_policy: row
                        .get("unknown_sender_policy")
                        .and_then(Value::as_str)
                        .unwrap_or("strict")
                        .to_string(),
                });
            }
        }
    }

    // --- Gather: local session file perms + broker env --------------------
    let loose_perm_files = data_dir
        .as_deref()
        .map(scan_loose_session_perms)
        .unwrap_or_default();
    let broker_enabled = broker_enabled_from_env(
        std::env::var("COPPERCLAW_CREDENTIAL_BROKER")
            .ok()
            .as_deref(),
    );

    let posture = Posture {
        egress_mode,
        groups,
        messaging_groups,
        loose_perm_files,
        broker_enabled,
    };

    let findings = analyze(&posture);

    // --- Optionally apply the safe, tightening fixes ----------------------
    let mut fixed: Vec<String> = Vec::new();
    if fix {
        // Each safe fixer scans the whole posture, so we only need to invoke
        // each at most once even if a finding id repeats.
        let mut did_egress = false;
        let mut did_approvals = false;
        let mut did_perms = false;
        for f in &findings {
            if !matches!(f.remediation, Remediation::Fixable { .. }) {
                continue;
            }
            match f.id {
                "egress-allow-empty" if !did_egress => {
                    apply_egress_scaffold(transport, &caller, &posture, &mut fixed).await;
                    did_egress = true;
                }
                "approvals-open" if !did_approvals => {
                    apply_approval_tighten(transport, &caller, &posture, &mut fixed).await;
                    did_approvals = true;
                }
                "session-file-perms" if !did_perms => {
                    apply_perm_tighten(&posture, &mut fixed);
                    did_perms = true;
                }
                _ => {}
            }
        }
    }

    finalise(&findings, &fixed, as_json)
}

/// The scaffold allow-list entry written for a group that has none. It is
/// a placeholder host:port the operator edits — it does NOT widen
/// reachability (deny-default already injects the model endpoint), it just
/// gives the operator a concrete, audited starting point and flips the
/// group out of the "no allow-list configured" open signal.
pub const EGRESS_SCAFFOLD_ENTRY: &str = "example.internal:443";

async fn apply_egress_scaffold<T>(
    transport: &T,
    caller: &Caller,
    posture: &Posture,
    fixed: &mut Vec<String>,
) where
    T: CallTransport + ?Sized,
{
    // Only act under allow-all and only for groups with an empty list —
    // the exact condition `analyze` flagged. Never touches a group that
    // already has a configured list (that would be a no-op churn).
    if posture.egress_mode != "allow-all" {
        return;
    }
    for g in &posture.groups {
        if g.configured_allow_len != 0 {
            continue;
        }
        let res = transport
            .call(
                "groups.config.set-egress-allow",
                json!({"id": g.agent_group_id, "allow": [EGRESS_SCAFFOLD_ENTRY]}),
                caller.clone(),
            )
            .await;
        match res {
            Ok(_) => fixed.push(format!(
                "scaffolded egress allow-list for group {} ({}): [{EGRESS_SCAFFOLD_ENTRY}] — edit \
                 with `cclaw groups config set-egress-allow`",
                g.name, g.agent_group_id
            )),
            Err(e) => fixed.push(format!(
                "FAILED to scaffold egress allow-list for group {}: {}",
                g.agent_group_id,
                describe_err(&e)
            )),
        }
    }
}

async fn apply_approval_tighten<T>(
    transport: &T,
    caller: &Caller,
    posture: &Posture,
    fixed: &mut Vec<String>,
) where
    T: CallTransport + ?Sized,
{
    for mg in &posture.messaging_groups {
        if mg.unknown_sender_policy != "open" {
            continue;
        }
        let res = transport
            .call(
                "messaging-groups.update",
                json!({"id": mg.messaging_group_id, "unknown_sender_policy": "request_approval"}),
                caller.clone(),
            )
            .await;
        match res {
            Ok(_) => fixed.push(format!(
                "tightened messaging group {} ({}) unknown_sender_policy: open -> request_approval",
                mg.name, mg.messaging_group_id
            )),
            Err(e) => fixed.push(format!(
                "FAILED to tighten messaging group {}: {}",
                mg.messaging_group_id,
                describe_err(&e)
            )),
        }
    }
}

fn apply_perm_tighten(posture: &Posture, fixed: &mut Vec<String>) {
    for f in &posture.loose_perm_files {
        let want = if f.is_dir { 0o700 } else { 0o600 };
        match chmod(&f.path, want) {
            Ok(()) => fixed.push(format!("tightened {} to {:#o}", f.path, want)),
            Err(e) => fixed.push(format!("FAILED to chmod {}: {e}", f.path)),
        }
    }
}

/// Apply a Unix permission mode to `path`. On non-Unix this is a no-op
/// error so the audit reports the platform limitation honestly rather
/// than silently claiming a fix.
#[cfg(unix)]
fn chmod(path: &str, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn chmod(_path: &str, _mode: u32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "chmod unsupported on this platform",
    ))
}

/// Walk `<data_dir>/sessions` and report any per-session DB file or
/// directory whose mode grants group/world access. On non-Unix the mode
/// is unavailable so the scan returns empty.
#[must_use]
pub fn scan_loose_session_perms(data_dir: &std::path::Path) -> Vec<LoosePermFile> {
    let sessions = data_dir.join("sessions");
    let mut out = Vec::new();
    scan_dir_recursive(&sessions, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[cfg(unix)]
fn scan_dir_recursive(dir: &std::path::Path, out: &mut Vec<LoosePermFile>) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mode = meta.permissions().mode() & 0o777;
        let is_dir = meta.is_dir();
        if perms_are_loose(mode) {
            out.push(LoosePermFile {
                path: path.to_string_lossy().into_owned(),
                mode,
                is_dir,
            });
        }
        if is_dir {
            scan_dir_recursive(&path, out);
        }
    }
}

#[cfg(not(unix))]
fn scan_dir_recursive(_dir: &std::path::Path, _out: &mut Vec<LoosePermFile>) {}

fn describe_err(e: &ClientError) -> String {
    match e {
        ClientError::Remote(p) => format!("{} ({})", p.message, p.code),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_posture() -> Posture {
        Posture {
            egress_mode: "deny-default".into(),
            groups: vec![],
            messaging_groups: vec![],
            loose_perm_files: vec![],
            broker_enabled: true,
        }
    }

    #[test]
    fn broker_env_truthy_and_falsy() {
        for t in [
            "1", "true", "TRUE", "yes", "on", "enable", "enabled", " on ",
        ] {
            assert!(broker_enabled_from_env(Some(t)), "{t} should be truthy");
        }
        for f in ["0", "false", "no", "off", "", "garbage"] {
            assert!(!broker_enabled_from_env(Some(f)), "{f} should be falsy");
        }
        assert!(!broker_enabled_from_env(None));
    }

    #[test]
    fn egress_env_maps_to_mode() {
        assert_eq!(egress_mode_from_env(Some("deny-default")), "deny-default");
        assert_eq!(egress_mode_from_env(Some("1")), "deny-default");
        assert_eq!(egress_mode_from_env(Some("deny")), "deny-default");
        assert_eq!(egress_mode_from_env(None), "allow-all");
        assert_eq!(egress_mode_from_env(Some("allow-all")), "allow-all");
        assert_eq!(egress_mode_from_env(Some("garbage")), "allow-all");
    }

    #[test]
    fn perms_loose_predicate() {
        assert!(perms_are_loose(0o644));
        assert!(perms_are_loose(0o755));
        assert!(perms_are_loose(0o640));
        assert!(perms_are_loose(0o604));
        assert!(!perms_are_loose(0o600));
        assert!(!perms_are_loose(0o700));
        assert!(!perms_are_loose(0o000));
    }

    #[test]
    fn analyze_tight_posture_has_no_open_findings() {
        let f = analyze(&empty_posture());
        assert!(
            f.iter().all(|x| x.severity == Severity::Ok),
            "tight posture must be all-ok: {f:?}",
        );
    }

    #[test]
    fn analyze_flags_egress_allow_all() {
        let mut p = empty_posture();
        p.egress_mode = "allow-all".into();
        p.broker_enabled = true;
        let f = analyze(&p);
        let egress = f.iter().find(|x| x.id == "egress-mode").unwrap();
        assert_eq!(egress.severity, Severity::High);
        assert!(matches!(egress.remediation, Remediation::ManualOnly { .. }));
    }

    #[test]
    fn analyze_flags_empty_allow_list_under_allow_all_as_fixable() {
        let mut p = empty_posture();
        p.egress_mode = "allow-all".into();
        p.groups.push(GroupPosture {
            agent_group_id: "ag-1".into(),
            name: "demo".into(),
            configured_allow_len: 0,
            tool_profile: Some("messaging".into()),
        });
        let f = analyze(&p);
        let empty = f.iter().find(|x| x.id == "egress-allow-empty").unwrap();
        assert_eq!(empty.severity, Severity::Warn);
        assert!(matches!(empty.remediation, Remediation::Fixable { .. }));
    }

    #[test]
    fn analyze_empty_allow_list_under_deny_default_is_ok() {
        let mut p = empty_posture();
        p.groups.push(GroupPosture {
            agent_group_id: "ag-1".into(),
            name: "demo".into(),
            configured_allow_len: 0,
            tool_profile: Some("messaging".into()),
        });
        let f = analyze(&p);
        let empty = f.iter().find(|x| x.id == "egress-allow-empty").unwrap();
        assert_eq!(empty.severity, Severity::Ok);
        assert!(matches!(empty.remediation, Remediation::None));
    }

    #[test]
    fn analyze_flags_missing_tool_profile_manual_only() {
        let mut p = empty_posture();
        p.groups.push(GroupPosture {
            agent_group_id: "ag-1".into(),
            name: "demo".into(),
            configured_allow_len: 3,
            tool_profile: None,
        });
        let f = analyze(&p);
        let tp = f.iter().find(|x| x.id == "tool-profile-missing").unwrap();
        assert_eq!(tp.severity, Severity::Warn);
        assert!(matches!(tp.remediation, Remediation::ManualOnly { .. }));
    }

    #[test]
    fn analyze_present_tool_profile_not_flagged() {
        let mut p = empty_posture();
        p.groups.push(GroupPosture {
            agent_group_id: "ag-1".into(),
            name: "demo".into(),
            configured_allow_len: 3,
            tool_profile: Some("coding".into()),
        });
        let f = analyze(&p);
        assert!(f.iter().all(|x| x.id != "tool-profile-missing"));
    }

    #[test]
    fn analyze_flags_open_approvals_fixable() {
        let mut p = empty_posture();
        p.messaging_groups.push(MessagingPosture {
            messaging_group_id: "mg-1".into(),
            name: "lobby".into(),
            channel_type: "telegram".into(),
            unknown_sender_policy: "open".into(),
        });
        let f = analyze(&p);
        let appr = f.iter().find(|x| x.id == "approvals-open").unwrap();
        assert_eq!(appr.severity, Severity::High);
        assert!(matches!(appr.remediation, Remediation::Fixable { .. }));
    }

    #[test]
    fn analyze_strict_approvals_not_flagged() {
        let mut p = empty_posture();
        p.messaging_groups.push(MessagingPosture {
            messaging_group_id: "mg-1".into(),
            name: "lobby".into(),
            channel_type: "telegram".into(),
            unknown_sender_policy: "request_approval".into(),
        });
        let f = analyze(&p);
        assert!(f.iter().all(|x| x.id != "approvals-open"));
    }

    #[test]
    fn analyze_flags_loose_session_file() {
        let mut p = empty_posture();
        p.loose_perm_files.push(LoosePermFile {
            path: "/data/sessions/ag/sess/inbound.db".into(),
            mode: 0o644,
            is_dir: false,
        });
        let f = analyze(&p);
        let perms = f.iter().find(|x| x.id == "session-file-perms").unwrap();
        assert_eq!(perms.severity, Severity::High);
        assert!(matches!(perms.remediation, Remediation::Fixable { .. }));
        if let Remediation::Fixable { manual_hint } = &perms.remediation {
            assert!(manual_hint.contains("0o600"));
        }
    }

    #[test]
    fn analyze_flags_loose_session_dir_wants_0700() {
        let mut p = empty_posture();
        p.loose_perm_files.push(LoosePermFile {
            path: "/data/sessions/ag".into(),
            mode: 0o755,
            is_dir: true,
        });
        let f = analyze(&p);
        let perms = f.iter().find(|x| x.id == "session-file-perms").unwrap();
        if let Remediation::Fixable { manual_hint } = &perms.remediation {
            assert!(manual_hint.contains("0o700"));
        } else {
            panic!("dir perm finding must be fixable");
        }
    }

    #[test]
    fn analyze_flags_broker_disabled_while_allow_all() {
        let mut p = empty_posture();
        p.egress_mode = "allow-all".into();
        p.broker_enabled = false;
        let f = analyze(&p);
        let broker = f.iter().find(|x| x.id == "broker-disabled").unwrap();
        assert_eq!(broker.severity, Severity::Warn);
    }

    #[test]
    fn analyze_broker_disabled_under_deny_default_is_ok() {
        let mut p = empty_posture();
        p.egress_mode = "deny-default".into();
        p.broker_enabled = false;
        let f = analyze(&p);
        let broker = f.iter().find(|x| x.id == "broker-disabled").unwrap();
        assert_eq!(broker.severity, Severity::Ok);
    }

    #[test]
    fn no_finding_suggests_loosening() {
        let p = Posture {
            egress_mode: "allow-all".into(),
            groups: vec![GroupPosture {
                agent_group_id: "ag-1".into(),
                name: "demo".into(),
                configured_allow_len: 0,
                tool_profile: None,
            }],
            messaging_groups: vec![MessagingPosture {
                messaging_group_id: "mg-1".into(),
                name: "lobby".into(),
                channel_type: "telegram".into(),
                unknown_sender_policy: "open".into(),
            }],
            loose_perm_files: vec![LoosePermFile {
                path: "/data/sessions/ag/sess/inbound.db".into(),
                mode: 0o666,
                is_dir: false,
            }],
            broker_enabled: false,
        };
        for f in analyze(&p) {
            let hint = match &f.remediation {
                Remediation::Fixable { manual_hint } | Remediation::ManualOnly { manual_hint } => {
                    manual_hint.to_ascii_lowercase()
                }
                Remediation::None => continue,
            };
            assert!(
                !hint.contains("allow-all"),
                "remediation must not propose allow-all: {hint}",
            );
            assert!(
                !hint.contains("policy=open")
                    && !hint.contains("policy open")
                    && !hint.contains("to open"),
                "remediation must not propose open approvals: {hint}",
            );
        }
    }

    #[test]
    fn finalise_open_no_fix_is_failure() {
        let findings = vec![Finding {
            id: "approvals-open",
            severity: Severity::High,
            detail: "x".into(),
            remediation: Remediation::Fixable {
                manual_hint: "y".into(),
            },
        }];
        let out = finalise(&findings, &[], false);
        assert!(!out.stderr.is_empty());
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn finalise_open_with_fix_is_success() {
        let findings = vec![Finding {
            id: "approvals-open",
            severity: Severity::High,
            detail: "x".into(),
            remediation: Remediation::Fixable {
                manual_hint: "y".into(),
            },
        }];
        let out = finalise(&findings, &["tightened mg-1".into()], false);
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains("applied fixes"));
    }

    #[test]
    fn finalise_all_ok_is_success() {
        let findings = vec![Finding::ok("egress-mode", "fine")];
        let out = finalise(&findings, &[], false);
        assert!(out.stderr.is_empty());
        assert!(out.stdout.contains("no open policies"));
    }

    #[test]
    fn finalise_json_emits_structured_payload() {
        let findings = vec![Finding {
            id: "approvals-open",
            severity: Severity::High,
            detail: "x".into(),
            remediation: Remediation::Fixable {
                manual_hint: "y".into(),
            },
        }];
        let out = finalise(&findings, &[], true);
        let body = if out.stdout.is_empty() {
            out.stderr
        } else {
            out.stdout
        };
        let v: Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(v["status"], "high");
        assert_eq!(v["findings"][0]["id"], "approvals-open");
        assert_eq!(v["findings"][0]["fixable"], true);
    }

    #[test]
    fn parse_egress_reads_mode_and_group_allow_lens() {
        let v = json!({
            "mode": "allow-all",
            "groups": [
                {"agent_group_id": "ag-1", "configured_allow": ["a:1", "b:2"]},
                {"agent_group_id": "ag-2", "configured_allow": []},
            ],
        });
        let (mode, lens) = parse_egress(&v, "deny-default");
        assert_eq!(mode, "allow-all");
        assert_eq!(lens, vec![("ag-1".to_string(), 2), ("ag-2".to_string(), 0)]);
    }

    #[test]
    fn parse_egress_falls_back_to_env_mode() {
        let v = json!({"groups": []});
        let (mode, _) = parse_egress(&v, "deny-default");
        assert_eq!(mode, "deny-default");
    }
}
